# agent-gateway

Matrix ↔ Claude Code / OpenCode gateway via ACP (Agent Client Protocol).

## Prerequisites

- `claude-agent-acp`: `npm install -g @agentclientprotocol/claude-agent-acp`
- `opencode` (optional, for `/agent opencode`)

## Commands

```bash
cargo build
cargo check
cargo clippy
cargo fmt
cargo run                                 # reads ~/.config/agent-gateway/config.toml by default
cargo run -- --config <path>              # override config path
cargo run -- --debug                      # debug-level logging
cargo run -- --install-user-service       # install systemd --user service and exit
```

No tests exist (`tests/` is empty, `RUST_TEST_THREADS=1` in `.cargo/config.toml`).

## Service

- Service unit template: `utils/agent-gateway.service` (embedded at compile time via `include_str!`)
- Sample config: `utils/config.toml`

### systemd --user

```bash
# Install (writes unit to ~/.config/systemd/user/agent-gateway.service)
cargo run -- --install-user-service

# Enable and start
systemctl --user enable --now agent-gateway

# Restart
systemctl --user restart agent-gateway

# Stop
systemctl --user stop agent-gateway

# Logs
journalctl --user -u agent-gateway -f
```

## Config

`~/.config/agent-gateway/config.toml`. Example at `utils/config.toml`.

Data dirs (XDG-style):

| Var | Default |
|---|---|
| `STATE_DIRECTORY` | `~/.local/state/agent-gateway` |
| `CACHE_DIRECTORY` | `~/.cache/agent-gateway` |

## Architecture

**Key files:**
- `src/main.rs` — CLI args (clap), dir resolution → `MatrixBot::new()` → `bot.run().await`
- `src/bot/matrix.rs` — MatrixBot: event handlers, session management, streaming response loop, commands
- `src/agent.rs` — `AgentSession` trait, `AgentType` enum, `AgentRegistry` (lazy backends)
- `src/agent/cc.rs` — `ClaudeCode`: spawns `claude-agent-acp` via ACP tokio transport
- `src/agent/opencode.rs` — `OpenCodeAgent`: spawns `opencode acp` via ACP tokio transport
- `src/config.rs` — TOML config parsing (`GatewayConfig`, `MatrixConfig`)

### Message flow

1. `on_room_message` → `handle_command()` (locks sessions internally)
2. Plain text → `run_user_prompt()` → `get_or_create_session()` → `run_task()`
3. `get_or_create_session()`: reuses `agent_session` if available; otherwise tries `session/resume` first, falls back to `session/new`
4. New room with no agent → error. User must `/agent claude-code-acp` or `/agent opencode` first. No default.

### ACP flow

`send_input(text)` → `send_prompt(text)` (sync). Poll `query_delta()` in 1s loop (doubles as pacing). On done, send full accumulated output as single message. Permissions auto-approved.

### Room leave safety

`run_task()` checks `room.typing_notice(true)` — 403 means no longer joined, triggers immediate break.

### Session lifecycle

- **Session file**: `room_sessions.json` in state dir, persists `agent_session_id` for cross-restart resume via `session/resume`
- **`/setpwd <path>`**: updates pwd + triggers `session/resume` to recreate process
- **Kick/leave**: removes session from map (drops agent session, closes ACP)
- **`/reset`**: drops current agent session, clears `agent_session_id`

### Key return types

- `handle_command` → `Result<Option<String>, MatrixError>`: `Ok(None)` = not a command; `Ok(Some(reply))` = handled; `Err(e)` → sent as `"Error: {e}"`
- `AgentDelta` has one variant: `Text { output: String, done: bool }`
- `MatrixError` has `Agent` / `Io` variants

### Global duplicate instance protection

At startup queries device list; bails if another device named `agent-gateway` has `last_seen_ts < 60s`.

## Stale docs

- `README.md` — describes old `claude -p` architecture, not current ACP-based one.
- `CLAUDE.md` — stale copy of AGENTS.md. Edit AGENTS.md only.
