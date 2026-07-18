# LettrLabs Setup: Claude Code on our ChatGPT/Codex Subscription

Instructions for setting up CC-Adapter from this audited LettrLabs fork. Written
so you can point Claude Code (or any coding agent) at this file and say
"set this up for me" — or follow it by hand.

**What you get:** a per-terminal switch between backends. `claude` keeps using
Anthropic as normal; a session with `ANTHROPIC_BASE_URL` pointed at the local
proxy uses the company ChatGPT/Codex subscription instead. Nothing global
changes; sessions run side by side.

**Notes for AI agents following this guide:**
- Build from THIS fork's default branch (`audit-hardening`) — never download
  upstream's pre-built binaries. The fork is security-audited and carries
  required fixes (see git log).
- Never write API keys or tokens into files. The ChatGPT login is OAuth and
  the human must run it themselves (Step 4).
- Do not set `host = "0.0.0.0"` in the config — the proxy has no inbound auth.

## Prerequisites

- A ChatGPT account with Codex access (open https://chatgpt.com/codex once and
  make sure it works for you)
- Git, and the Rust toolchain (`cargo`). Windows: `winget install Rustlang.Rustup`;
  macOS/Linux: https://rustup.rs

## Step 1: Clone and build

```powershell
git clone https://github.com/lettrlabs/CC-Adapter.git
cd CC-Adapter
cargo build --release
```

## Step 2: Install the binary

Windows (PowerShell):

```powershell
New-Item -ItemType Directory -Force -Path C:\Tools\claude-adapter | Out-Null
Copy-Item target\release\claude-adapter.exe C:\Tools\claude-adapter\
# add to user PATH (new terminals only)
$p = [Environment]::GetEnvironmentVariable("Path", "User")
if ($p -notlike "*claude-adapter*") {
  [Environment]::SetEnvironmentVariable("Path", "$p;C:\Tools\claude-adapter", "User")
}
```

macOS/Linux: `cp target/release/claude-adapter ~/.local/bin/`

## Step 3: Create the config

Create `~/.config/claude-adapter/config.toml` (the adapter finds it there
automatically — no `--config` flag needed):

```toml
[server]
host = "127.0.0.1"
port = 8080
# Leave ~/.claude/settings.json alone; opt into the proxy per terminal instead.
manage_claude_settings = false
log_level = "info"
log_file_enabled = false

[providers.chatgpt]
type = "chatgpt"   # OAuth — no API key

[models]
default_provider = "chatgpt"
# The Codex model id. This is the label shown in the Codex app's model picker,
# lowercased with dashes (e.g. "5.6 Sol" -> gpt-5.6-sol). OpenAI renames these
# periodically; if requests start failing with "model is not supported", check
# the Codex app for the current label and update this line, then RESTART the
# adapter (hot-reload does not apply model changes).
default_model = "gpt-5.6-sol"
```

## Step 4: Log in (human step — agents must not do this)

```powershell
claude-adapter login
```

Your browser opens OpenAI's login. Sign in with the ChatGPT account that has
Codex. Tokens are stored locally in `~/.claude-adapter/` and auto-refresh.

## Step 5: Run it

Terminal 1 — start the proxy (leave it running; Ctrl+C to stop):

```powershell
claude-adapter serve
```

Terminal 2 — a Codex-backed Claude Code session:

```powershell
$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8080"
claude
```

(macOS/Linux: `ANTHROPIC_BASE_URL=http://127.0.0.1:8080 claude`)

Any terminal *without* that variable keeps using real Claude/Anthropic.

### Optional: one-word switcher

Add this to your PowerShell `$PROFILE`, then just type `claude-codex`:

```powershell
function claude-codex {
    if (-not (Get-Process claude-adapter -ErrorAction SilentlyContinue)) {
        Start-Process -WindowStyle Minimized "C:\Tools\claude-adapter\claude-adapter.exe" -ArgumentList "serve"
        Start-Sleep -Seconds 2
    }
    $prev = $env:ANTHROPIC_BASE_URL
    $env:ANTHROPIC_BASE_URL = "http://127.0.0.1:8080"
    try { claude @args }
    finally {
        if ($null -ne $prev) { $env:ANTHROPIC_BASE_URL = $prev }
        else { Remove-Item Env:ANTHROPIC_BASE_URL -ErrorAction SilentlyContinue }
    }
}
```

## Verify it works

In the Codex-backed session, ask anything. To sanity-check the backend:
`curl http://127.0.0.1:8080/health` should return `{"status":"ok"}` while the
proxy runs. (Don't ask the model who made it — Claude Code's system prompt
gives it the Claude persona, so it may say Anthropic even on GPT.)

## Troubleshooting

| Symptom | Fix |
|---|---|
| `model is not supported when using Codex with a ChatGPT account` | Model id is stale. Check the Codex app's model label, update `default_model`, restart the adapter. |
| `Provider specified in routing table not found` / `providers=[]` in startup log | Config didn't load. Confirm it's at `~/.config/claude-adapter/config.toml` (this fork fails loudly if missing — rebuild if yours doesn't). |
| `connection refused` from Claude Code | The proxy isn't running — start `claude-adapter serve`. |
| ChatGPT token expired | `claude-adapter login` again. |
| Config edits not taking effect | Restart the adapter. Hot-reload does not reliably apply model changes. |

## Things to know

- Codex-backed sessions send prompts/code to **OpenAI**, not Anthropic. Don't
  route customer-data work through it pending vendor review.
- Chinese-and-English log output is normal — the upstream author writes
  bilingual messages.
- This depends on OpenAI's unpublished internal API; expect occasional
  breakage after OpenAI ships changes. Report issues to Beau.
