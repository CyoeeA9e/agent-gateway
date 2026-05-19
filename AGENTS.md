# agent-gateway

Matrix + IRC ↔ OpenCode / Claude Code gateway via ACP.

## Commands

```bash
cargo check              # fast feedback
cargo clippy             # lint (pre-commit)
cargo fmt                # format
cargo run                # reads ~/.config/agent-gateway/config.toml
cargo run -- --debug     # debug logging
```

No tests (`tests/` empty).

## Config

`~/.config/agent-gateway/config.toml` — both Matrix and IRC sections optional:

```toml
[matrix]
id = "@bot:server"
password = "your_password"
allowed-user = ["@admin:server"]

[irc]
server = "chat.example.com"
port = 6697
tls = true
nick = "my-bot"
password = "server_pass"
channels = ["#general"]
allowed_users = []
```

Data dirs (XDG-style):

| Env var | Default |
|---|---|
| `STATE_DIRECTORY` | `~/.local/state/agent-gateway` |
| `CACHE_DIRECTORY` | `~/.cache/agent-gateway` |

## Key facts

- **Edition 2024** (`Cargo.toml`). `unsafe`/`impl Trait`/`if let` rules differ from 2021.
- **matrix-sdk 0.9** with `e2e-encryption`, `sqlite`, `native-tls`. Encrypted rooms are the norm.
- **`agent-client-protocol`** features `unstable_session_usage` and `unstable_session_resume`.
- **ACP backends are spawned subprocesses**: `claude-agent-acp` (no args) or `opencode acp` (args: `["acp"]`). Both must be in `PATH`.
- **ACP permissions auto-approved** — init handler selects the first option or Cancelled if none.
- **Default agent is `None`** — rooms/channels MUST run `/agent opencode` before prompts, or they return an error.
- **`query_delta()` polls with 100ms timeout** — returns `AgentDelta::Text { output, done }` or `AgentDelta::ToolCall { title, input }`.
- **`ToolCallUpdate` only forwarded when `raw_input` is non-empty** — transient `in_progress` updates are dropped.
- **`process_with_agent` delta loop** uses `loop` + `match` (NOT `while let Ok(Some(x))`) because `Ok(None)` is returned on timeout and must continue polling.
- **Session persistence**: `room_sessions.json` / `irc_sessions.json` in state dir stores `agent_session_id` for cross-restart resume via `session/resume`. On restart, ACP processes are gone → resume fails → falls back to `session/new`.
- **`room_sessions.json`** `#[serde(rename_all = "kebab-case")]` on `AgentType`: `"open-code"` not `"opencode"`, `"claude-code-acp"`, `"none"`.
- **IRC: in channels, @mention (`@nick`) is required** — bare `/command` or text is ignored. Formats: `@nick`, `nick:`, `nick,`, `nick `. Mention + `/command` works as command, otherwise treated as prompt. Case-insensitive.
- **IRC PING** must handle both `PING :payload` and `:server PING :payload` (prefixed variant).
- **IRC TLS** uses `danger_accept_invalid_certs(true)` with 10s connect + 15s handshake timeouts.
- **Matrix encrypted messages**: `on_encrypted_message` queues event IDs → key stream listener (spawned via `room_keys_received_stream()`) fetches with `room.event()` and re-queues if still encrypted.
- **Duplicate instance check**: `GET /devices` (15s timeout, non-fatal). Finds other devices named `agent-gateway` with `last_seen_ts < 60s`.
- **Room leave detection**: `run_task()` calls `room.typing_notice(true)` each poll cycle; HTTP 403 → immediate break.
- **`StrippedRoomMemberEvent` handler** takes 3 params: `(event, room, client)` — the only handler that receives `Client`.

## Architecture

- `main.rs` — clap CLI, resolve dirs, spawn IRC bot as tokio task, run Matrix bot in main task.
- A failed Matrix login doesn't kill the process — falls through to `ctrl_c().await`.
- Both bots create their own `AgentRegistry` (no sharing).

### File layout

```
src/
  main.rs, lib.rs         — entrypoint, module re-exports
  config.rs               — GatewayConfig (matrix + optional irc)
  agent.rs                — AgentSession trait, AgentType (None|ClaudeCodeAcp|OpenCode), AgentRegistry
  agent/acp.rs            — AcpBackend + AcpSession: subprocess spawn, ACP transport
  agent/cc.rs             — ClaudeCode(AcpBackend): spawns `claude-agent-acp`
  agent/opencode.rs       — OpenCode(AcpBackend): spawns `opencode acp`
  bot.rs                  — mod irc, mod matrix
  bot/matrix.rs           — Matrix bot: invites, encrypted/plain messages, commands, session management
  bot/irc.rs              — IRC bot: TLS connect, PRIVMSG parsing, @mention dispatch, ACP bridge
```

## Code style

- **`tokio::sync::Mutex as AsyncMutex`** — never import bare `tokio::sync::Mutex`.
- **Guard clauses first** — every handler starts with `if`/`let else` early-return.
- **`let Ok(x) = fallible else { return/continue }`** and `let Some(x) = optional else { … }` over nested `match`.
- **`loop` + `match`** for ACP delta polling.

## Stale docs

- `README.md` — describes old `claude -p` architecture, not ACP-based. Edit AGENTS.md only.
- `CLAUDE.md` — stale copy of this file. Edit AGENTS.md only.
