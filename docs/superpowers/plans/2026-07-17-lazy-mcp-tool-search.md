# Lazy MCP Tool Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve Claude Code's complete MCP catalog while forwarding only non-deferred and locally discovered tool schemas to the ChatGPT Codex model.

**Architecture:** Claude Code remains the MCP host and executor. CC-Adapter parses Anthropic `defer_loading` and `tool_reference` metadata, reconstructs the loaded tool set from request history, and filters the Responses API tool array without storing server-side session state. The adapter also configures Claude Code to use gateway authentication and Tool Search whenever automatic settings management is enabled.

**Tech Stack:** Rust 2024, serde/serde_json, anyhow, tracing, Axum, Cargo unit tests, Claude Code CLI.

## Global Constraints

- Keep MCP configuration, credentials, permissions, and execution in Claude Code.
- Do not require ChatGPT-side MCP setup.
- Do not connect CC-Adapter directly to MCP servers.
- Do not use OpenAI hosted `tool_search` in this implementation.
- Preserve all-tools behavior for clients that omit `defer_loading`.
- Preserve non-ChatGPT provider behavior beyond accepting the new Anthropic fields.
- Use test-first red-green cycles for every Rust behavior change.
- Preserve and restore every Claude setting changed by automatic settings management.

---

## File Map

- `src/types/anthropic.rs`: Anthropic request wire types, including deferred definitions and tool references.
- `src/convert/request_responses.rs`: Stateless history scan, tool selection, tool-result conversion, and tool-count diagnostics for ChatGPT Responses requests.
- `src/claude_settings.rs`: Pure JSON transformation functions for managed Claude Code environment values and backup compatibility.
- `src/main.rs`: File I/O around Claude settings injection/restoration and operator-facing startup guidance.
- `README.md`: User behavior, managed variables, deferred-tool conversion, and manual setup instructions.
- `docs/agent-install.md`: Installation-agent requirements for the new settings.
- `config-example.toml`: Configuration comment describing the managed environment.

### Task 1: Parse Anthropic Deferred-Tool Metadata

**Files:**
- Modify: `src/types/anthropic.rs`

**Interfaces:**
- Produces: `ToolDefinition.defer_loading: Option<bool>` for the Responses converter.
- Produces: `ContentBlock::ToolReference { tool_name: String }` for nested Tool Search results.

- [ ] **Step 1: Write the failing deserialization test**

Add this test module to `src/types/anthropic.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_deferred_tool_and_nested_tool_reference() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_search",
                    "content": [{
                        "type": "tool_reference",
                        "tool_name": "mcp__playwright__browser_navigate"
                    }]
                }]
            }],
            "tools": [{
                "name": "mcp__playwright__browser_navigate",
                "description": "Navigate a browser",
                "input_schema": {"type": "object"},
                "defer_loading": true
            }]
        })).unwrap();

        let tool = &request.tools.as_ref().unwrap()[0];
        assert_eq!(tool.defer_loading, Some(true));

        let MessageContent::Blocks(message_blocks) = &request.messages[0].content else {
            panic!("expected structured message content");
        };
        let ContentBlock::ToolResult { content: Some(ToolResultContent::Blocks(result)), .. } =
            &message_blocks[0]
        else {
            panic!("expected structured tool result");
        };
        assert!(matches!(
            &result[0],
            ContentBlock::ToolReference { tool_name }
                if tool_name == "mcp__playwright__browser_navigate"
        ));
    }
}
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```powershell
cargo test types::anthropic::tests::deserializes_deferred_tool_and_nested_tool_reference
```

Expected: compilation fails because `defer_loading` and `ToolReference` do not exist.

- [ ] **Step 3: Add the minimal wire fields**

Add this variant to `ContentBlock`:

```rust
#[serde(rename = "tool_reference")]
ToolReference {
    tool_name: String,
},
```

Add this field to `ToolDefinition`:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub defer_loading: Option<bool>,
```

Update existing `ToolDefinition` literals in tests to include `defer_loading: None`.

- [ ] **Step 4: Run focused and full tests and verify GREEN**

Run:

```powershell
cargo test types::anthropic::tests::deserializes_deferred_tool_and_nested_tool_reference
cargo test
```

Expected: focused test passes; full suite reports zero failures.

- [ ] **Step 5: Commit**

```powershell
git add src/types/anthropic.rs src/convert/request.rs src/convert/request_responses.rs
git commit -m "feat(protocol): parse deferred tool metadata"
```

### Task 2: Filter Deferred Tools from Codex Context

**Files:**
- Modify: `src/convert/request_responses.rs`

**Interfaces:**
- Consumes: `ToolDefinition.defer_loading` and `ContentBlock::ToolReference` from Task 1.
- Produces: `collect_referenced_tool_names(messages: &[Message]) -> HashSet<String>`.
- Produces: `convert_tools(tools: &[ToolDefinition], referenced: &HashSet<String>) -> Vec<ResponsesTool>`.
- Produces: deterministic tool-reference summaries in `function_call_output`.

- [ ] **Step 1: Write a failing initial-catalog filtering test**

Add a test that creates one non-deferred `ToolSearch` definition and 100 deferred MCP definitions, calls `convert_request_to_responses`, and asserts that only `ToolSearch` is forwarded:

```rust
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
```

- [ ] **Step 2: Run the filtering test and verify RED**

Run:

```powershell
cargo test convert::request_responses::tests::omits_large_deferred_catalog_before_discovery
```

Expected: assertion fails because all 101 tools are forwarded.

- [ ] **Step 3: Implement history scanning and minimal selection**

Add `use std::collections::HashSet;` and these helpers. They recurse through nested tool results, collect every `ToolReference.tool_name`, and preserve catalog order during selection:

```rust
fn collect_referenced_tool_names(messages: &[Message]) -> HashSet<String> {
    let mut referenced = HashSet::new();
    for message in messages {
        if let MessageContent::Blocks(blocks) = &message.content {
            collect_references_from_blocks(blocks, &mut referenced);
        }
    }
    referenced
}

fn collect_references_from_blocks(blocks: &[ContentBlock], referenced: &mut HashSet<String>) {
    for block in blocks {
        match block {
            ContentBlock::ToolReference { tool_name } => {
                referenced.insert(tool_name.clone());
            }
            ContentBlock::ToolResult {
                content: Some(ToolResultContent::Blocks(nested)),
                ..
            } => collect_references_from_blocks(nested, referenced),
            _ => {}
        }
    }
}

fn convert_tools(
    tools: &[ToolDefinition],
    referenced: &HashSet<String>,
) -> Vec<ResponsesTool> {
    tools
        .iter()
        .filter(|tool| {
            tool.defer_loading != Some(true) || referenced.contains(&tool.name)
        })
        .map(|tool| ResponsesTool {
            tool_type: "function".to_string(),
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: Some(tool.input_schema.clone()),
        })
        .collect()
}
```

Emit one `debug!` record with total, deferred, referenced, and forwarded counts.

- [ ] **Step 4: Run the filtering test and verify GREEN**

Run:

```powershell
cargo test convert::request_responses::tests::omits_large_deferred_catalog_before_discovery
```

Expected: test passes with one forwarded tool.

- [ ] **Step 5: Write failing discovery and compatibility tests**

Add these helpers and separate tests:

```rust
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
        vec![test_tool("ToolSearch", None), test_tool("tool_2", Some(true))],
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
```

- [ ] **Step 6: Run the new tests and verify RED**

Run:

```powershell
cargo test convert::request_responses::tests::loads_only_referenced_deferred_tools
cargo test convert::request_responses::tests::tool_reference_result_becomes_readable_function_output
cargo test convert::request_responses::tests::forwards_all_tools_when_defer_loading_is_absent
cargo test convert::request_responses::tests::prior_history_references_remain_loaded
cargo test convert::request_responses::tests::unknown_reference_does_not_fail_or_forward_an_unmatched_schema
```

Expected: discovery and summary tests fail until recursive extraction and output rendering are complete.

- [ ] **Step 7: Complete recursive extraction and result rendering**

Implement a block renderer that preserves text and appends one deterministic line for references:

```rust
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
```

Unknown references remain in the textual summary but do not add a schema unless a matching definition exists.

- [ ] **Step 8: Run focused and full tests and verify GREEN**

Run:

```powershell
cargo test convert::request_responses::tests
cargo test
```

Expected: all request-converter tests and the full suite pass.

- [ ] **Step 9: Commit**

```powershell
git add src/convert/request_responses.rs
git commit -m "feat(chatgpt): defer undiscovered MCP tools"
```

### Task 3: Force Gateway Authentication and Tool Search Safely

**Files:**
- Create: `src/claude_settings.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `apply_managed_env(settings: &mut Value, proxy_url: &str, stream_idle_timeout_ms: u64) -> Value`.
- Produces: `restore_managed_env(settings: &mut Value, backup: Option<&Value>)`.
- Consumes: pure transformations from `inject_claude_settings` and `restore_claude_settings`.

- [ ] **Step 1: Write failing pure settings tests**

Add `mod claude_settings;` beside the existing module declarations in `src/main.rs`, then create `src/claude_settings.rs` with tests first. The tests must assert:

```rust
#[test]
fn applies_gateway_auth_and_tool_search_with_exact_backup() {
    let mut settings = json!({
        "env": {
            "ANTHROPIC_API_KEY": "existing-key",
            "ENABLE_TOOL_SEARCH": "auto:5",
            "UNRELATED": "keep"
        }
    });

    let backup = apply_managed_env(
        &mut settings,
        "http://127.0.0.1:8080",
        300_000,
    );

    assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8080");
    assert_eq!(settings["env"]["ANTHROPIC_API_KEY"], "cc-adapter-local");
    assert_eq!(settings["env"]["ENABLE_TOOL_SEARCH"], "true");
    assert_eq!(settings["env"]["CLAUDE_STREAM_IDLE_TIMEOUT_MS"], "300000");
    assert_eq!(settings["env"]["UNRELATED"], "keep");
    assert_eq!(backup["anthropic_api_key"]["old_value"], "existing-key");
    assert_eq!(backup["enable_tool_search"]["old_value"], "auto:5");
}

#[test]
fn restore_reinstates_existing_values_and_removes_new_values() {
    let original = json!({
        "env": {
            "ANTHROPIC_API_KEY": "existing-key",
            "ENABLE_TOOL_SEARCH": "auto:5",
            "UNRELATED": "keep"
        },
        "permissions": {"allow": ["Read"]}
    });
    let mut settings = original.clone();
    let backup = apply_managed_env(
        &mut settings,
        "http://127.0.0.1:8080",
        300_000,
    );
    restore_managed_env(&mut settings, Some(&backup));
    assert_eq!(settings, original);
}

#[test]
fn zero_timeout_leaves_existing_timeout_unmanaged() {
    let mut settings = json!({
        "env": {"CLAUDE_STREAM_IDLE_TIMEOUT_MS": "900000"}
    });
    let backup = apply_managed_env(
        &mut settings,
        "http://127.0.0.1:8080",
        0,
    );
    assert_eq!(settings["env"]["CLAUDE_STREAM_IDLE_TIMEOUT_MS"], "900000");
    assert!(backup.get("claude_stream_idle_timeout_ms").is_none());
}

#[test]
fn restores_legacy_base_url_backup() {
    let mut settings = json!({"env": {"ANTHROPIC_BASE_URL": "http://adapter"}});
    let legacy = json!({"had_value": true, "old_value": "https://old.example"});
    restore_managed_env(&mut settings, Some(&legacy));
    assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "https://old.example");
}
```

Declare the wished-for function signatures with `unimplemented!()` bodies so the initial failure is caused by missing behavior:

```rust
pub fn apply_managed_env(
    _settings: &mut Value,
    _proxy_url: &str,
    _stream_idle_timeout_ms: u64,
) -> Value {
    unimplemented!()
}

pub fn restore_managed_env(_settings: &mut Value, _backup: Option<&Value>) {
    unimplemented!()
}
```

- [ ] **Step 2: Run settings tests and verify RED**

Run:

```powershell
cargo test claude_settings::tests
```

Expected: compilation or assertions fail because the pure transformations are not implemented or wired.

- [ ] **Step 3: Implement minimal pure transformations**

Replace the `unimplemented!()` bodies with these exact managed values and transformations:

```rust
use serde_json::{json, Map, Value};

const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const TOOL_SEARCH_ENV: &str = "ENABLE_TOOL_SEARCH";
const STREAM_TIMEOUT_ENV: &str = "CLAUDE_STREAM_IDLE_TIMEOUT_MS";

pub const LOCAL_API_KEY: &str = "cc-adapter-local";
pub const TOOL_SEARCH_ENABLED: &str = "true";

fn backup_entry(settings: &Value, key: &str) -> Value {
    let old_value = settings
        .get("env")
        .and_then(|env| env.get(key))
        .cloned();
    json!({
        "had_value": old_value.is_some(),
        "old_value": old_value,
    })
}

fn env_mut(settings: &mut Value) -> &mut Map<String, Value> {
    if !settings.get("env").is_some_and(Value::is_object) {
        settings["env"] = json!({});
    }
    settings["env"].as_object_mut().unwrap()
}

pub fn apply_managed_env(
    settings: &mut Value,
    proxy_url: &str,
    stream_idle_timeout_ms: u64,
) -> Value {
    let mut backup = Map::new();
    backup.insert("anthropic_base_url".to_string(), backup_entry(settings, BASE_URL_ENV));
    backup.insert("anthropic_api_key".to_string(), backup_entry(settings, API_KEY_ENV));
    backup.insert("enable_tool_search".to_string(), backup_entry(settings, TOOL_SEARCH_ENV));
    if stream_idle_timeout_ms > 0 {
        backup.insert(
            "claude_stream_idle_timeout_ms".to_string(),
            backup_entry(settings, STREAM_TIMEOUT_ENV),
        );
    }

    let env = env_mut(settings);
    env.insert(BASE_URL_ENV.to_string(), Value::String(proxy_url.to_string()));
    env.insert(API_KEY_ENV.to_string(), Value::String(LOCAL_API_KEY.to_string()));
    env.insert(
        TOOL_SEARCH_ENV.to_string(),
        Value::String(TOOL_SEARCH_ENABLED.to_string()),
    );
    if stream_idle_timeout_ms > 0 {
        env.insert(
            STREAM_TIMEOUT_ENV.to_string(),
            Value::String(stream_idle_timeout_ms.to_string()),
        );
    }

    Value::Object(backup)
}

fn restore_section(env: &mut Map<String, Value>, key: &str, section: Option<&Value>) {
    let Some(section) = section else { return };
    match section.get("had_value").and_then(Value::as_bool) {
        Some(true) => {
            if let Some(old_value) = section.get("old_value") {
                env.insert(key.to_string(), old_value.clone());
            }
        }
        Some(false) => {
            env.remove(key);
        }
        None => {}
    }
}

pub fn restore_managed_env(settings: &mut Value, backup: Option<&Value>) {
    let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) else {
        return;
    };

    match backup {
        None => {
            env.remove(BASE_URL_ENV);
            if env.get(API_KEY_ENV).and_then(Value::as_str) == Some(LOCAL_API_KEY) {
                env.remove(API_KEY_ENV);
            }
            if env.get(TOOL_SEARCH_ENV).and_then(Value::as_str) == Some(TOOL_SEARCH_ENABLED) {
                env.remove(TOOL_SEARCH_ENV);
            }
        }
        Some(backup) => {
            let base = backup
                .get("anthropic_base_url")
                .or_else(|| backup.get("had_value").is_some().then_some(backup));
            restore_section(env, BASE_URL_ENV, base);
            restore_section(env, API_KEY_ENV, backup.get("anthropic_api_key"));
            restore_section(env, TOOL_SEARCH_ENV, backup.get("enable_tool_search"));
            restore_section(
                env,
                STREAM_TIMEOUT_ENV,
                backup.get("claude_stream_idle_timeout_ms"),
            );
        }
    }

    if env.is_empty() {
        settings.as_object_mut().unwrap().remove("env");
    }
}
```

- [ ] **Step 4: Run settings tests and verify GREEN**

Run:

```powershell
cargo test claude_settings::tests
```

Expected: all settings transformation tests pass.

- [ ] **Step 5: Wire file I/O to the tested transformations**

Add `mod claude_settings;` in `src/main.rs`. Replace the inline backup construction and restore section logic with:

```rust
let backup = claude_settings::apply_managed_env(
    &mut settings,
    &proxy_url,
    stream_idle_timeout_ms,
);
```

and:

```rust
claude_settings::restore_managed_env(&mut settings, backup.as_ref());
```

Keep atomic temp-file writes and backup-file cleanup in `main.rs`.

Update startup messages to list `ANTHROPIC_API_KEY` and `ENABLE_TOOL_SEARCH`. When automatic management is disabled, print all three required per-shell variables.

- [ ] **Step 6: Run focused and full tests and verify GREEN**

Run:

```powershell
cargo test claude_settings::tests
cargo test
```

Expected: settings tests and full suite pass.

- [ ] **Step 7: Commit**

```powershell
git add src/claude_settings.rs src/main.rs
git commit -m "fix(settings): force gateway tool search"
```

### Task 4: Document the Lazy MCP Execution Boundary

**Files:**
- Modify: `README.md`
- Modify: `docs/agent-install.md`
- Modify: `config-example.toml`

**Interfaces:**
- Documents the settings and request behavior implemented in Tasks 1-3.

- [ ] **Step 1: Update user-facing documentation**

Document these exact facts:

```text
Claude Code owns MCP connections, credentials, permissions, and execution.
CC-Adapter receives the complete deferred catalog on the wire but forwards only
non-deferred and tool_reference-selected schemas to Codex.
No ChatGPT connector setup is required.
Automatic settings management injects ANTHROPIC_BASE_URL, ANTHROPIC_API_KEY,
ENABLE_TOOL_SEARCH, and optionally CLAUDE_STREAM_IDLE_TIMEOUT_MS, then restores
their prior values on normal shutdown.
```

For `manage_claude_settings=false`, provide PowerShell and POSIX examples using:

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8080"
$env:ANTHROPIC_API_KEY = "cc-adapter-local"
$env:ENABLE_TOOL_SEARCH = "true"
claude
```

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080
export ANTHROPIC_API_KEY=cc-adapter-local
export ENABLE_TOOL_SEARCH=true
claude
```

- [ ] **Step 2: Check documentation consistency**

Run:

```powershell
rg -n "ANTHROPIC_BASE_URL|ANTHROPIC_API_KEY|ENABLE_TOOL_SEARCH|defer_loading|tool_reference" README.md docs/agent-install.md config-example.toml
git diff --check
```

Expected: every managed setting and lazy-tool term is documented; diff check exits zero.

- [ ] **Step 3: Commit**

```powershell
git add README.md docs/agent-install.md config-example.toml
git commit -m "docs: explain lazy MCP routing"
```

### Task 5: Verify Repository and Live MCP Round Trip

**Files:**
- Modify only if verification exposes a tested defect.

**Interfaces:**
- Consumes the completed adapter binary and the user's existing Claude Code MCP configuration.
- Produces fresh build, test, lint, and live round-trip evidence.

- [ ] **Step 1: Run formatting, tests, and lint**

Run:

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: each command exits zero; tests report zero failures; clippy reports no warnings.

- [ ] **Step 2: Start the feature adapter on an isolated port**

Use the existing ChatGPT provider configuration without modifying global Claude settings:

```powershell
$adapterConfig = Join-Path $env:USERPROFILE ".config\claude-adapter\config.toml"
cargo run -- serve --config $adapterConfig --port 18081
```

If that configuration has `manage_claude_settings=true`, create a temporary copy with the field set to `false` and pass the temporary path. Do not edit the user's canonical config.

Expected: `/health` on port 18081 returns `{"status":"ok"}`.

- [ ] **Step 3: Run a safe Claude Code MCP smoke test**

In a separate PowerShell process set only process-local variables and allow only the Playwright read/navigation tools:

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:18081"
$env:ANTHROPIC_API_KEY = "cc-adapter-local"
$env:ENABLE_TOOL_SEARCH = "true"
claude -p "Use the configured Playwright MCP server to open https://example.com and report the page title. You must call the MCP tool." --allowedTools "mcp__plugin_playwright_playwright__browser_navigate,mcp__plugin_playwright_playwright__browser_snapshot"
```

Expected: Claude Code invokes `ToolSearch`, the adapter forwards only discovered Playwright schemas, Playwright opens `example.com`, and the final answer reports `Example Domain` without a context-window error.

- [ ] **Step 4: Inspect tool-count evidence**

Run the adapter at debug log level for the smoke request and confirm the converter's structured log reports:

```text
total_tools > forwarded_tools
deferred_tools > 0
referenced_tools > 0 after ToolSearch
```

Do not print request credentials or token-store contents.

- [ ] **Step 5: Re-run final verification after any smoke-test fix**

Run:

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Expected: all commands exit zero.

- [ ] **Step 6: Commit any final tested corrections**

If the smoke test required a code correction, stage only that correction and its regression test, then commit:

```powershell
git commit -m "fix(chatgpt): complete lazy MCP round trip"
```

If no correction was needed, do not create an empty commit.
