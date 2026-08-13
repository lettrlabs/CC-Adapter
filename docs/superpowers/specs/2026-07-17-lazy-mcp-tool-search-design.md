# Lazy MCP Tool Search Bridge Design

## Problem

Claude Code normally keeps large MCP catalogs out of the model's initial context by marking MCP tools with `defer_loading: true` and using its local `ToolSearch` tool to return `tool_reference` blocks on demand. Claude Code disables that behavior when `ANTHROPIC_BASE_URL` points to a third-party proxy unless `ENABLE_TOOL_SEARCH=true` is set.

CC-Adapter currently compounds that fallback in two ways:

- Its Anthropic request types discard `defer_loading` and cannot deserialize `tool_reference` content blocks.
- Its ChatGPT Responses converter forwards every received tool schema as an immediately available function.

With the full LettrLabs MCP catalog, those tool schemas plus Claude Code's system prompt exceed the Codex model's rendered context before the model can answer or call a tool.

## Goals

- Keep every existing Claude Code MCP server connected and callable.
- Keep MCP configuration, credentials, permissions, and execution in Claude Code.
- Exclude deferred MCP schemas from the initial Codex model context.
- Load only the schemas selected by Claude Code's `ToolSearch` results.
- Make ChatGPT/Codex inference independent of Claude subscription usage limits.
- Preserve existing behavior for clients that do not send deferred tools.
- Restore all user settings changed by the adapter when it shuts down normally.

## Non-goals

- Do not copy MCP connections or credentials into ChatGPT.
- Do not make CC-Adapter connect to or execute MCP servers.
- Do not use OpenAI's hosted `tool_search` in the first implementation.
- Do not change non-ChatGPT provider behavior beyond accepting the additional Anthropic fields.
- Do not redesign Claude Code permissions or bypass tool approval policies.

## Architecture

Claude Code remains the agent runtime and MCP host. CC-Adapter is a stateless protocol bridge.

On every Anthropic Messages request, CC-Adapter will scan the supplied conversation history for `tool_reference` blocks. It will convert only:

1. Tools without `defer_loading: true`.
2. Deferred tools whose names were referenced anywhere in the conversation history.

All other deferred tool definitions remain present on the Claude Code-to-adapter wire request but are omitted from the adapter-to-Codex `tools` array, so they do not consume model context.

The adapter will not retain a server-side tool registry. Claude Code resends the full catalog and conversation history on each request, so the selected tool set can be reconstructed deterministically and survives adapter restarts.

## Request Data Flow

### Initial request

1. Claude Code connects to all configured MCP servers and receives their tool catalogs.
2. Claude Code sends its built-in tools plus MCP definitions marked `defer_loading: true` to CC-Adapter.
3. CC-Adapter forwards only non-deferred tools, including Claude Code's local `ToolSearch` tool, to Codex.
4. Codex calls `ToolSearch` when it needs a capability that is not currently loaded.
5. CC-Adapter translates that normal function call back into an Anthropic `tool_use` block.

### Discovery request

1. Claude Code executes `ToolSearch` locally against its connected MCP catalog.
2. Claude Code sends a `tool_result` whose content includes one or more `tool_reference` blocks.
3. CC-Adapter converts the tool result to an OpenAI `function_call_output` containing a concise loaded-tool summary.
4. CC-Adapter adds the referenced definitions, and only those definitions, to the Codex `tools` array.
5. Codex calls the selected MCP tool as an ordinary function.
6. CC-Adapter returns an Anthropic `tool_use`; Claude Code executes it through the existing MCP connection and returns the result.

Previously referenced tools remain loaded because CC-Adapter scans the complete message history on subsequent requests.

## Protocol Changes

The Anthropic request model will gain:

- `ToolDefinition.defer_loading: Option<bool>`.
- A `ContentBlock::ToolReference { tool_name }` variant.

The Responses request converter will gain pure helpers that:

- Recursively collect referenced tool names from message and nested tool-result content.
- Select non-deferred and referenced tools while preserving their original order.
- Render tool-reference-only results as a deterministic text summary for the paired `function_call_output`.

An unknown reference will not crash conversion. It will be excluded unless a matching definition exists, and conversion will continue so stale or dynamically changing catalogs do not break the session.

## Claude Code Settings

When `manage_claude_settings=true`, the adapter manages three environment keys in `~/.claude/settings.json`:

- `ANTHROPIC_BASE_URL=http://<adapter-host>:<port>` routes inference through CC-Adapter.
- `ENABLE_TOOL_SEARCH=true` overrides Claude Code's third-party-base-URL fallback and enables deferred MCP discovery.
- `CLAUDE_STREAM_IDLE_TIMEOUT_MS=<configured value>` is optional and is left unmanaged when configured as `0`.

Automatic mode manages only `ANTHROPIC_BASE_URL`, `ENABLE_TOOL_SEARCH=true`, and optional `CLAUDE_STREAM_IDLE_TIMEOUT_MS`; it leaves `ANTHROPIC_API_KEY` untouched so signed-in Claude.ai OAuth and hosted connectors remain available.

The backup file preserves the previous value and presence state for every key the adapter changes. Normal shutdown restores those values exactly. Existing backup formats remain readable, including legacy `anthropic_api_key` sections. Without a backup ownership record, restore is a complete no-op.

When `manage_claude_settings=false`, startup documentation and console guidance recommend only `ANTHROPIC_BASE_URL` and `ENABLE_TOOL_SEARCH=true`, with the timeout remaining optional.

Optional fallback for a Claude Code client that is not signed in: `ANTHROPIC_API_KEY=cc-adapter-local`. Setting any API key takes precedence over your Claude.ai login and disables Claude.ai-hosted connectors; local/configured MCP servers still work.

## Compatibility and Failure Handling

- Requests without `defer_loading` preserve the current behavior and forward all tools.
- A deferred tool becomes available only after a matching `tool_reference` appears in history.
- Empty or malformed tool-reference results still produce a valid paired function output.
- Existing Anthropic-compatible providers continue receiving the original request fields through serialization.
- Existing settings backups containing only `ANTHROPIC_BASE_URL` and timeout sections remain restorable.
- The adapter will log total, deferred, referenced, and forwarded tool counts without logging credentials.

## Security Boundaries

- MCP server commands, URLs, OAuth tokens, API keys, and environment variables stay in Claude Code and its MCP processes.
- Codex receives tool names, descriptions, JSON schemas, calls, and results only when required by the conversation.
- The optional manual placeholder Anthropic API key is not an Anthropic credential and must never be used as the ChatGPT upstream bearer token. When set, it disables Claude.ai-hosted connectors by taking precedence over Claude.ai login.
- ChatGPT OAuth continues to be loaded exclusively from CC-Adapter's token store.

## Testing

Unit tests will prove the red-green behavior for:

- Deserializing `defer_loading` and nested `tool_reference` blocks.
- Omitting a large deferred catalog on the initial request.
- Loading only referenced deferred tools on the discovery request.
- Keeping prior references loaded across later turns.
- Preserving all-tools behavior when no definitions are deferred.
- Producing a readable `function_call_output` for tool-reference results.
- Injecting and restoring all managed Claude Code settings, including pre-existing values.
- Restoring legacy backup formats.

Repository verification will run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets --all-features -- -D warnings`.

A live smoke test will run the feature branch adapter on a separate local port with explicit per-process environment variables, launch Claude Code non-interactively, and require it to discover and call one safe read-only configured MCP tool. The test must show that the request reaches Codex without the context-window failure and that the MCP result returns to the model.

## Acceptance Criteria

- A synthetic request with at least 100 deferred MCP tools forwards none of them before discovery.
- Non-deferred Claude Code tools remain available on the initial turn.
- A `tool_reference` loads exactly its matching deferred definition.
- The referenced MCP tool can be called and its result can continue the conversation.
- Existing non-deferred clients retain their current tool behavior.
- Adapter-managed Claude settings force gateway auth and lazy tool search, then restore prior values on normal shutdown.
- No ChatGPT-side MCP setup is required.
- All repository formatting, tests, and lint checks pass.
