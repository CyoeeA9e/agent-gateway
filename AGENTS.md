# agent-gateway

Matrix → Claude Code / OpenCode gateway via ACP (Agent Client Protocol).

## Prerequisites

- `claude-agent-acp`: `npm install -g @agentclientprotocol/claude-agent-acp`
- `opencode` (optional, for `/agent opencode`)

## Commands

```bash
cargo build
cargo check
cargo clippy
cargo fmt
cargo run                     # reads ~/.config/agent-gateway/config.toml by default
cargo run -- --config <path>  # override config path
cargo run -- --debug          # debug-level logging
```

No tests exist. `tests/` is empty.

## Restart

```bash
pkill -x agent-gateway 2>/dev/null; sleep 1; nohup cargo run > /tmp/gateway.log 2>&1 &
```

`pkill -x` matches exact binary name, NOT `cargo` wrapper. Do not use `pkill -f cargo`.

## Config

`~/.config/agent-gateway/config.toml`:
```toml
[matrix]
id = "@bot:server"
password = "..."
allowed-user = ["@admin:server"]
```

Data dirs (XDG-style):

| Var | Default |
|---|---|
| `STATE_DIRECTORY` | `~/.local/state/agent-gateway` |
| `CACHE_DIRECTORY` | `~/.cache/agent-gateway` |

## Architecture

- **`src/agent.rs`** — `AgentSession` trait, `AgentType` enum, `AgentRegistry` (manages both backends lazily)
- **`src/agent/cc.rs`** — `ClaudeCode`: spawns `claude-agent-acp`
- **`src/agent/opencode.rs`** — `OpenCodeAgent`: spawns `opencode acp`
- **`src/bot/matrix.rs`** — `MatrixBot`: event handlers, session management, streaming response loop, commands
- **`src/config.rs`** — TOML config parsing
- **`src/main.rs`** — CLI args, dir resolution → `MatrixBot::new()` → `bot.run().await`

### Message flow

1. `on_room_message` → `handle_command()` (locks sessions internally)
2. Plain text → `run_user_prompt()` → `get_or_create_session()` → `run_task()`
3. `get_or_create_session()`: reuses `agent_session` if available; otherwise tries `session/resume` first (if `agent_session_id` exists), falls back to `session/new`
4. New room with no agent → error. User must `/agent claude-code-acp` or `/agent opencode` first. No default.

### Streaming responses

`run_task()` polls `query_delta()` in a loop (1s timeout doubles as pacing). When done, sends full accumulated output as a single one-shot message. No incremental edits.

### Room leave safety

`run_task()` checks `room.typing_notice(true)` — 403 means no longer joined, triggers immediate break.

### Session lifecycle

- **Session file**: `room_sessions.json` in state dir, persists `agent_session_id` for cross-restart resume
- **Cross-restart resume**: `session/resume` attempted first; falls back to `session/new`
- **`/setpwd <path>`**: updates `Session.pwd`, triggers `session/resume` with new cwd → backend recreates process on fingerprint change
- **Kick/leave**: session entry removed from map (drops `agent_session`, closing ACP session)
- **`/reset`**: drops current agent session, clears `agent_session_id`

### ACP flow

1. `send_input(text)` → `session.send_prompt(text)` (sync)
2. `query_delta().await` → `session.read_update()` with 1s timeout
3. `run_task()` polls `query_delta()` in a loop
4. Permissions auto-approved

### Global duplicate instance protection

At startup queries device list; bails if another device named `agent-gateway` has `last_seen_ts < 60s`.

## Important

- `CLAUDE.md` is a stale copy of this file. Edit AGENTS.md only.
- `README.md` is stale — describes old `claude -p` architecture, not current ACP-based one.
- `MatrixError` (`bot::matrix`) has `Agent` / `Io` variants, used as `handle_command` return type.
- `handle_command` returns `Result<Option<String>, MatrixError>`. `Ok(None)` = not a command; `Ok(Some(reply))` = handled; `Err(e)` = sent to room as `"Error: {e}"`.
- `AgentDelta` has one variant: `Text { output: String, done: bool }`.
