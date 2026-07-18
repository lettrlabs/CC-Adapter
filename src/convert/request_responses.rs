use std::collections::HashSet;

use anyhow::Result;
use tracing::debug;

use crate::types::anthropic::{
    ContentBlock, Message, MessageContent, MessagesRequest, SystemPrompt, ToolDefinition,
    ToolResultContent,
};
use crate::types::responses::{
    InputContent, InputContentPart, InputItem, ReasoningConfig, ResponsesRequest, ResponsesTool,
    TextConfig,
};

/// ChatGPT Codex `codex/responses` 要求請求必須帶非空 `instructions`（對應 system）
/// ChatGPT Codex requires a non-empty `instructions` field (maps from system prompt)
const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are a helpful assistant.";

/// 將 Anthropic Messages API 請求轉換為 OpenAI Responses API 請求
/// Convert an Anthropic Messages API request into an OpenAI Responses API request
pub fn convert_request_to_responses(
    req: MessagesRequest,
    resolved_model: &str,
) -> Result<ResponsesRequest> {
    let model = resolved_model.to_string();

    // 提取系統提示作為 instructions（缺省或全空白時填預設，否則 Codex 回 HTTP 400）
    // Extract system prompt as instructions (default if missing/blank, else Codex returns HTTP 400)
    let from_system = req.system.as_ref().map(|s| match s {
        SystemPrompt::Text(text) => text.clone(),
        SystemPrompt::Blocks(blocks) => blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    });
    let instructions = match from_system {
        Some(ref s) if !s.trim().is_empty() => s.clone(),
        _ => DEFAULT_CODEX_INSTRUCTIONS.to_string(),
    };

    let mut input: Vec<InputItem> = Vec::new();

    // 轉換每條 Anthropic 訊息為 Responses API 的 input items
    // Convert each Anthropic message into Responses API input items
    for msg in &req.messages {
        convert_message_to_input(msg, &mut input)?;
    }

    let referenced = collect_referenced_tool_names(&req.messages);
    let tools = req
        .tools
        .as_ref()
        .map(|tools| convert_tools(tools, &referenced));

    Ok(ResponsesRequest {
        model,
        input,
        store: false,
        stream: true,
        instructions,
        tools,
        reasoning: Some(ReasoningConfig {
            effort: Some("medium".to_string()),
            summary: Some("auto".to_string()),
        }),
        text: Some(TextConfig {
            verbosity: Some("medium".to_string()),
        }),
        // 勿要求 reasoning.encrypted_content：Codex 常把可讀回覆壓進加密區塊，導致 message 正文為空、Claude Code 無內容
        // Avoid requesting reasoning.encrypted_content: Codex often hides visible text there, leaving message body empty
        include: None,
    })
}

/// 將單一 Anthropic 訊息轉換為 Responses API input items
/// Convert a single Anthropic message into Responses API input items
fn convert_message_to_input(msg: &Message, out: &mut Vec<InputItem>) -> Result<()> {
    match &msg.content {
        MessageContent::Text(text) => {
            out.push(InputItem::Message {
                role: normalize_input_role(&msg.role),
                content: InputContent::Text(text.clone()),
            });
        }
        MessageContent::Blocks(blocks) => {
            convert_blocks_to_input(&msg.role, blocks, out)?;
        }
    }
    Ok(())
}

/// ChatGPT Codex 的 input 只接受 user/assistant 角色；Claude Code 會在 messages
/// 內夾帶 role="system" 訊息（例如 SessionStart hook 內容），Codex 對此回
/// HTTP 400 "System messages are not allowed"。將非 assistant 角色一律映射為
/// user，保留其在對話中的位置。
/// ChatGPT Codex only accepts user/assistant roles in `input`; Claude Code
/// embeds role="system" messages in `messages` (e.g. SessionStart hook
/// context), which Codex rejects with HTTP 400 "System messages are not
/// allowed". Map any non-assistant role to user, preserving conversation
/// position.
fn normalize_input_role(role: &str) -> String {
    if role == "assistant" {
        "assistant".to_string()
    } else {
        "user".to_string()
    }
}

/// 轉換 Anthropic 內容區塊為 Responses API input items
/// Convert Anthropic content blocks into Responses API input items
fn convert_blocks_to_input(
    role: &str,
    blocks: &[ContentBlock],
    out: &mut Vec<InputItem>,
) -> Result<()> {
    match role {
        "assistant" => convert_assistant_blocks(blocks, out),
        // system（或其他未知角色）一律走 user 路徑，避免 Codex 拒收 system 訊息
        // Route system (or any unknown role) through the user path — Codex
        // rejects system messages in `input`
        _ => convert_user_blocks(blocks, out),
    }
}

/// 轉換 assistant 區塊：文字 → message，tool_use → function_call
/// Convert assistant blocks: text → message, tool_use → function_call
fn convert_assistant_blocks(blocks: &[ContentBlock], out: &mut Vec<InputItem>) -> Result<()> {
    let mut text_parts: Vec<String> = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                text_parts.push(text.clone());
            }
            ContentBlock::ToolUse { id, name, input } => {
                // 先輸出累積的文字
                // Flush accumulated text first
                if !text_parts.is_empty() {
                    out.push(InputItem::Message {
                        role: "assistant".to_string(),
                        content: InputContent::Text(text_parts.join("")),
                    });
                    text_parts.clear();
                }
                out.push(InputItem::FunctionCall {
                    name: name.clone(),
                    arguments: serde_json::to_string(input)?,
                    call_id: id.clone(),
                });
            }
            ContentBlock::Thinking { .. } => {}
            _ => {}
        }
    }

    if !text_parts.is_empty() {
        out.push(InputItem::Message {
            role: "assistant".to_string(),
            content: InputContent::Text(text_parts.join("")),
        });
    }

    Ok(())
}

/// 轉換 user 區塊：文字/圖片 → message，tool_result → function_call_output
/// Convert user blocks: text/image → message, tool_result → function_call_output
fn convert_user_blocks(blocks: &[ContentBlock], out: &mut Vec<InputItem>) -> Result<()> {
    let mut content_parts: Vec<InputContentPart> = Vec::new();
    let mut tool_results: Vec<(String, String, bool)> = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                content_parts.push(InputContentPart::Text { text: text.clone() });
            }
            ContentBlock::Image { source } => {
                let data_url = format!("data:{};base64,{}", source.media_type, source.data);
                content_parts.push(InputContentPart::Image {
                    image_url: data_url,
                    detail: Some("auto".to_string()),
                });
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let text = match content {
                    Some(ToolResultContent::Text(t)) => t.clone(),
                    Some(ToolResultContent::Blocks(inner_blocks)) => {
                        extract_tool_result_text(inner_blocks)
                    }
                    None => String::new(),
                };
                let is_err = is_error.unwrap_or(false);
                let output = if is_err {
                    format!("Error: {}", text)
                } else {
                    text
                };
                tool_results.push((tool_use_id.clone(), output, is_err));
            }
            _ => {}
        }
    }

    // 輸出使用者文字/圖片訊息
    // Emit user text/image message
    if !content_parts.is_empty() {
        let content = if content_parts.len() == 1 {
            if let InputContentPart::Text { text } = &content_parts[0] {
                InputContent::Text(text.clone())
            } else {
                InputContent::Parts(content_parts)
            }
        } else {
            InputContent::Parts(content_parts)
        };

        out.push(InputItem::Message {
            role: "user".to_string(),
            content,
        });
    }

    // 每個 tool_result → function_call_output
    for (call_id, output, _) in tool_results {
        out.push(InputItem::FunctionCallOutput { call_id, output });
    }

    Ok(())
}

fn extract_tool_result_text(blocks: &[ContentBlock]) -> String {
    let mut text = String::new();
    let mut references = Vec::new();
    collect_tool_result_parts(blocks, &mut text, &mut references);
    if !references.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("Loaded tools: ");
        text.push_str(&references.join(", "));
    }
    text
}

fn collect_tool_result_parts(
    blocks: &[ContentBlock],
    text: &mut String,
    references: &mut Vec<String>,
) {
    for block in blocks {
        match block {
            ContentBlock::Text { text: block_text } => text.push_str(block_text),
            ContentBlock::ToolReference { tool_name } => references.push(tool_name.clone()),
            ContentBlock::ToolResult {
                content: Some(ToolResultContent::Text(nested_text)),
                ..
            } => text.push_str(nested_text),
            ContentBlock::ToolResult {
                content: Some(ToolResultContent::Blocks(nested)),
                ..
            } => collect_tool_result_parts(nested, text, references),
            _ => {}
        }
    }
}

fn collect_referenced_tool_names(messages: &[Message]) -> HashSet<String> {
    let mut tool_search_uses = HashSet::new();
    let mut referenced = HashSet::new();
    for message in messages {
        if let MessageContent::Blocks(blocks) = &message.content {
            collect_references_from_history_blocks(
                blocks,
                false,
                &mut tool_search_uses,
                &mut referenced,
            );
        }
    }
    referenced
}

fn collect_references_from_history_blocks(
    blocks: &[ContentBlock],
    accept_direct_references: bool,
    tool_search_uses: &mut HashSet<String>,
    referenced: &mut HashSet<String>,
) {
    for block in blocks {
        match block {
            ContentBlock::ToolReference { tool_name } if accept_direct_references => {
                referenced.insert(tool_name.clone());
            }
            ContentBlock::ToolUse { id, name, .. } if name == "ToolSearch" => {
                tool_search_uses.insert(id.clone());
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content: Some(ToolResultContent::Blocks(nested)),
                is_error,
            } => collect_references_from_history_blocks(
                nested,
                is_error != &Some(true) && tool_search_uses.contains(tool_use_id),
                tool_search_uses,
                referenced,
            ),
            _ => {}
        }
    }
}

/// 轉換 Anthropic 工具定義為 Responses API 工具格式
/// Convert Anthropic tool definitions to Responses API tool format
fn convert_tools(tools: &[ToolDefinition], referenced: &HashSet<String>) -> Vec<ResponsesTool> {
    let deferred = tools
        .iter()
        .filter(|tool| tool.defer_loading == Some(true))
        .count();
    let converted = tools
        .iter()
        .filter(|tool| tool.defer_loading != Some(true) || referenced.contains(&tool.name))
        .map(|tool| ResponsesTool {
            tool_type: "function".to_string(),
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: Some(tool.input_schema.clone()),
        })
        .collect::<Vec<_>>();

    debug!(
        total = tools.len(),
        deferred,
        referenced = referenced.len(),
        forwarded = converted.len(),
        "Filtered deferred tools for ChatGPT Responses request"
    );

    converted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_tool(name: &str, defer_loading: Option<bool>) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: Some(format!("Tool {name}")),
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading,
        }
    }

    fn test_request(messages: Vec<Message>, tools: Vec<ToolDefinition>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages,
            system: None,
            tools: Some(tools),
            tool_choice: None,
            stream: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
        }
    }

    fn discovery_history(references: &[&str]) -> Vec<Message> {
        vec![
            Message {
                role: "user".to_string(),
                content: MessageContent::Text("Find a tool".to_string()),
            },
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "toolu_search".to_string(),
                    name: "ToolSearch".to_string(),
                    input: json!({"query": "browser"}),
                }]),
            },
            Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_search".to_string(),
                    content: Some(ToolResultContent::Blocks(
                        references
                            .iter()
                            .map(|name| ContentBlock::ToolReference {
                                tool_name: (*name).to_string(),
                            })
                            .collect(),
                    )),
                    is_error: None,
                }]),
            },
        ]
    }

    #[test]
    fn omits_large_deferred_catalog_before_discovery() {
        let mut tools = vec![ToolDefinition {
            name: "ToolSearch".to_string(),
            description: Some("Search locally available tools".to_string()),
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
        }];
        tools.extend((0..100).map(|index| ToolDefinition {
            name: format!("mcp__server__tool_{index}"),
            description: Some(format!("Deferred MCP tool {index}")),
            input_schema: json!({"type": "object", "properties": {"id": {"type": "string"}}}),
            cache_control: None,
            defer_loading: Some(true),
        }));

        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text("Navigate to example.com".to_string()),
            }],
            system: None,
            tools: Some(tools),
            tool_choice: None,
            stream: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
        };

        let converted = convert_request_to_responses(req, "gpt-5.6-sol").unwrap();
        let forwarded = converted.tools.unwrap();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].name, "ToolSearch");
    }

    #[test]
    fn loads_only_referenced_deferred_tools() {
        let request = test_request(
            discovery_history(&["tool_7", "tool_2"]),
            vec![
                test_tool("ToolSearch", None),
                test_tool("tool_2", Some(true)),
                test_tool("tool_7", Some(true)),
                test_tool("tool_9", Some(true)),
            ],
        );
        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        let names: Vec<_> = converted
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, vec!["ToolSearch", "tool_2", "tool_7"]);
    }

    #[test]
    fn tool_reference_result_becomes_readable_function_output() {
        let request = test_request(
            discovery_history(&["tool_7", "tool_2"]),
            vec![
                test_tool("ToolSearch", None),
                test_tool("tool_2", Some(true)),
                test_tool("tool_7", Some(true)),
            ],
        );
        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        let output = converted.input.iter().find_map(|item| match item {
            InputItem::FunctionCallOutput { call_id, output } if call_id == "toolu_search" => {
                Some(output.as_str())
            }
            _ => None,
        });
        assert_eq!(output, Some("Loaded tools: tool_7, tool_2"));
    }

    #[test]
    fn forwards_all_tools_when_defer_loading_is_absent() {
        let request = test_request(
            vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text("Use a legacy tool".to_string()),
            }],
            vec![
                test_tool("legacy_1", None),
                test_tool("legacy_2", None),
                test_tool("legacy_3", None),
            ],
        );
        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        assert_eq!(converted.tools.unwrap().len(), 3);
    }

    #[test]
    fn prior_history_references_remain_loaded() {
        let mut messages = discovery_history(&["tool_2"]);
        messages.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::Text("I found the tool.".to_string()),
        });
        messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::Text("Use it again.".to_string()),
        });
        let request = test_request(
            messages,
            vec![
                test_tool("ToolSearch", None),
                test_tool("tool_2", Some(true)),
            ],
        );
        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        let names: Vec<_> = converted
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, vec!["ToolSearch", "tool_2"]);
    }

    #[test]
    fn unknown_reference_does_not_fail_or_forward_an_unmatched_schema() {
        let request = test_request(
            discovery_history(&["tool_missing"]),
            vec![
                test_tool("ToolSearch", None),
                test_tool("tool_known", Some(true)),
            ],
        );
        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        let names: Vec<_> = converted
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, vec!["ToolSearch"]);
        assert!(converted.input.iter().any(|item| matches!(
            item,
            InputItem::FunctionCallOutput { output, .. }
                if output == "Loaded tools: tool_missing"
        )));
    }

    #[test]
    fn top_level_tool_reference_does_not_unlock_a_deferred_tool() {
        let request = test_request(
            vec![Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolReference {
                    tool_name: "tool_2".to_string(),
                }]),
            }],
            vec![
                test_tool("ToolSearch", None),
                test_tool("tool_2", Some(true)),
            ],
        );

        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        let names = converted
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["ToolSearch"]);
    }

    #[test]
    fn unrelated_tool_result_does_not_unlock_a_deferred_tool() {
        let messages = vec![
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "toolu_other".to_string(),
                    name: "LookupSomethingElse".to_string(),
                    input: json!({}),
                }]),
            },
            Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_other".to_string(),
                    content: Some(ToolResultContent::Blocks(vec![
                        ContentBlock::ToolReference {
                            tool_name: "tool_2".to_string(),
                        },
                    ])),
                    is_error: None,
                }]),
            },
        ];
        let request = test_request(
            messages,
            vec![
                test_tool("ToolSearch", None),
                test_tool("tool_2", Some(true)),
            ],
        );

        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        let names = converted
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["ToolSearch"]);
    }

    #[test]
    fn errored_tool_search_result_does_not_unlock_a_deferred_tool() {
        let mut messages = discovery_history(&["tool_2"]);
        let MessageContent::Blocks(result_blocks) = &mut messages[2].content else {
            panic!("expected tool result blocks");
        };
        let ContentBlock::ToolResult { is_error, .. } = &mut result_blocks[0] else {
            panic!("expected tool result");
        };
        *is_error = Some(true);
        let request = test_request(
            messages,
            vec![
                test_tool("ToolSearch", None),
                test_tool("tool_2", Some(true)),
            ],
        );

        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        let names = converted
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["ToolSearch"]);
    }

    #[test]
    fn preserves_text_and_renders_nested_tool_references() {
        let messages = vec![
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: "toolu_search".to_string(),
                        name: "ToolSearch".to_string(),
                        input: json!({"query": "browser"}),
                    },
                    ContentBlock::ToolUse {
                        id: "nested_search".to_string(),
                        name: "ToolSearch".to_string(),
                        input: json!({"query": "nested browser"}),
                    },
                ]),
            },
            Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_search".to_string(),
                    content: Some(ToolResultContent::Blocks(vec![
                        ContentBlock::Text {
                            text: "Matches found.".to_string(),
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "nested_search".to_string(),
                            content: Some(ToolResultContent::Blocks(vec![
                                ContentBlock::ToolReference {
                                    tool_name: "tool_2".to_string(),
                                },
                            ])),
                            is_error: None,
                        },
                    ])),
                    is_error: None,
                }]),
            },
        ];
        let request = test_request(
            messages,
            vec![
                test_tool("ToolSearch", None),
                test_tool("tool_2", Some(true)),
            ],
        );

        let converted = convert_request_to_responses(request, "gpt-5.6-sol").unwrap();
        let output = converted.input.iter().find_map(|item| match item {
            InputItem::FunctionCallOutput { call_id, output } if call_id == "toolu_search" => {
                Some(output.as_str())
            }
            _ => None,
        });
        assert_eq!(output, Some("Matches found.\nLoaded tools: tool_2"));
        let names = converted
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["ToolSearch", "tool_2"]);
    }

    #[test]
    fn test_basic_text_conversion() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: MessageContent::Text("Hello".to_string()),
            }],
            system: Some(SystemPrompt::Text("You are helpful.".to_string())),
            tools: None,
            tool_choice: None,
            stream: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
        };

        let result = convert_request_to_responses(req, "gpt-5-codex").unwrap();

        assert_eq!(result.model, "gpt-5-codex");
        assert_eq!(result.instructions, "You are helpful.");
        assert!(!result.store);
        assert!(result.stream);
        assert_eq!(result.input.len(), 1);
    }

    #[test]
    fn test_system_role_messages_become_user() {
        // Claude Code 會在 messages 內夾帶 role="system" 訊息；Codex 拒收 system，
        // 必須映射為 user。
        // Claude Code embeds role="system" messages in `messages`; Codex rejects
        // system roles in `input`, so they must be mapped to user.
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: MessageContent::Text("Hello".to_string()),
                },
                Message {
                    role: "system".to_string(),
                    content: MessageContent::Text("SessionStart hook context".to_string()),
                },
                Message {
                    role: "system".to_string(),
                    content: MessageContent::Blocks(vec![ContentBlock::Text {
                        text: "block-style system message".to_string(),
                    }]),
                },
            ],
            system: None,
            tools: None,
            tool_choice: None,
            stream: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
        };

        let result = convert_request_to_responses(req, "gpt-5.6-sol").unwrap();

        assert_eq!(result.input.len(), 3);
        for item in &result.input {
            if let InputItem::Message { role, .. } = item {
                assert_ne!(role, "system", "system role must not reach Codex input");
            }
        }
    }

    #[test]
    fn test_tool_use_conversion() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: MessageContent::Text("What's the weather?".to_string()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Text {
                            text: "Let me check.".to_string(),
                        },
                        ContentBlock::ToolUse {
                            id: "toolu_01".to_string(),
                            name: "get_weather".to_string(),
                            input: json!({"location": "SF"}),
                        },
                    ]),
                },
                Message {
                    role: "user".to_string(),
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "toolu_01".to_string(),
                        content: Some(ToolResultContent::Text("72F sunny".to_string())),
                        is_error: None,
                    }]),
                },
            ],
            system: None,
            tools: Some(vec![ToolDefinition {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                input_schema: json!({"type": "object", "properties": {"location": {"type": "string"}}}),
                cache_control: None,
                defer_loading: None,
            }]),
            tool_choice: None,
            stream: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
        };

        let result = convert_request_to_responses(req, "gpt-5-codex").unwrap();

        // 應產生：user message、assistant message、function_call、function_call_output
        // Should produce: user message, assistant message, function_call, function_call_output
        assert_eq!(result.input.len(), 4);
        assert!(result.tools.is_some());
        assert_eq!(result.instructions, DEFAULT_CODEX_INSTRUCTIONS);
    }

    #[test]
    fn test_thinking_blocks_are_dropped() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Visible text".to_string(),
                    },
                    ContentBlock::Thinking {
                        thinking: "internal reasoning".to_string(),
                        signature: None,
                    },
                ]),
            }],
            system: None,
            tools: None,
            tool_choice: None,
            stream: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            metadata: None,
        };

        let result = convert_request_to_responses(req, "gpt-5-codex").unwrap();

        assert_eq!(result.input.len(), 1);
        match &result.input[0] {
            InputItem::Message { role, content } => {
                assert_eq!(role, "assistant");
                match content {
                    InputContent::Text(text) => assert_eq!(text, "Visible text"),
                    _ => panic!("expected text content"),
                }
            }
            _ => panic!("expected message input item"),
        }
    }
}
