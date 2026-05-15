# agent-gateway

Matrix ↔ Claude Code / OpenCode gateway via ACP (Agent Client Protocol).

## Prerequisites

```bash
npm install -g @agentclientprotocol/claude-agent-acp   # required for `/agent claude-code-acp`
# opencode in PATH — optional, for `/agent opencode`
```

## Commands

```bash
cargo check                    # fast feedback
cargo clippy                   # lint (pre-commit)
cargo fmt                      # format
cargo run                      # reads ~/.config/agent-gateway/config.toml
cargo run -- --debug           # debug logging
cargo run -- --install-user-service
```

No tests (`tests/` empty).

## Config

`~/.config/agent-gateway/config.toml`:
```toml
[matrix]
id = "@bot:server"
password = "your_password"
allowed-user = ["@admin:server"]
```

Data dirs (XDG-style):

| Env var | Default |
|---|---|
| `STATE_DIRECTORY` | `~/.local/state/agent-gateway` |
| `CACHE_DIRECTORY` | `~/.cache/agent-gateway` |

## Key facts an agent is likely to miss

- **Edition 2024** (`Cargo.toml`). The `unsafe`/`impl Trait`/`if let` rules differ from 2021.
- **matrix-sdk 0.9** with `e2e-encryption`, `sqlite`, `native-tls`. Encrypted rooms are the norm.
- **`agent-client-protocol`** with features `unstable_session_usage` and `unstable_session_resume`.
- **ACP backends are spawned subprocesses**: `claude-agent-acp` (no args) or `opencode acp` (args: `["acp"]`). Both must be in `PATH` at runtime.
- **ACP permissions auto-approved** — the init handler responds `Selected` for the first option or `Cancelled` if none.
- **`query_delta()` polls with 1s timeout** — doubles as pacing. Returns `AgentDelta::Text { output, done }` or `AgentDelta::ToolCall { title, input }`.
- **Default agent is `None`** — new rooms MUST run `/agent claude-code-acp` or `/agent opencode` first, or prompts return an error.
- **Encrypted message flow**: `on_encrypted_message` queues event IDs → key stream listener (spawned via `room_keys_received_stream()`) fetches with `room.event()` and re-queues if still encrypted after decryption attempt.
- **Duplicate instance check**: `GET /devices` with 15s timeout, non-fatal on failure. Finds other devices named `agent-gateway` with `last_seen_ts < 60s`.
- **Room leave detection**: `run_task()` calls `room.typing_notice(true)` each poll cycle; HTTP 403 → immediate break.
- **Session persistence**: `room_sessions.json` in state dir stores `agent_session_id` for cross-restart resume via `session/resume`. On restart, previously running ACP processes are gone → resume fails → falls back to `session/new`.
- **`room_sessions.json`** has `#[serde(rename_all = "kebab-case")]` on `AgentType`: `"open-code"` not `"opencode"`, `"claude-code-acp"`, `"none"`.
- **`/setpwd <path>`**: canonicalizes the path, updates session pwd, and attempts `session/resume` to recreate the agent process in the new directory.
- **`/reset`**: drops `agent_session` and clears `agent_session_id`; next message creates fresh session.
- **State store + crypto store** use SQLite at `STATE_DIRECTORY/matrix_store/`.
- **`StrippedRoomMemberEvent` handler** takes 3 params: `(event, room, client)` — the only handler that receives `Client`.

## Architecture

- `src/main.rs` — clap CLI → `MatrixBot::new()` → `bot.run().await`
- `src/bot/matrix.rs` — `run()` split into: `build_client()`, `login_or_restore()`, `check_duplicate_instance()`, `register_event_handlers()`, `spawn_key_stream_listener()`, `run_sync_loop()`, `shutdown()`
- `src/agent.rs` — `AgentSession` trait, `AgentType` enum, `AgentRegistry` (lazy backends)
- `src/agent/acp.rs` — `AcpBackend` + `AcpSession`: spawns subprocess, ACP tokio transport, `send_prompt`/`read_update` loop
- `src/agent/cc.rs` — `ClaudeCode(AcpBackend)`: spawns `claude-agent-acp`
- `src/agent/opencode.rs` — `OpenCode(AcpBackend)`: spawns `opencode acp`

## Code style

Conventions extracted from the codebase, in order of likelihood an agent would guess wrong:

- **`tokio::sync::Mutex as AsyncMutex`** — alias used throughout to distinguish from `std::sync::Mutex`. Never import `tokio::sync::Mutex` bare.
- **Guard clauses first** — every handler starts with `if`/`let else` early-return for filters and edge cases. Normal path follows flatly.
- **`while let Ok(Some(x)) = expr`** preferred over `loop { match expr { Ok(Some(x)) => …, Ok(None) => {}, Err(_) => break } }`.
- **`let Ok(x) = fallible else { return/continue }`** and **`let Some(x) = optional else { … }`** preferred over nested `match` for error/None handling.

## Stale docs

- `README.md` — describes the old `claude -p` architecture (not ACP-based).
- `CLAUDE.md` — stale copy of this file. Edit AGENTS.md only.
