# agent-gateway

Matrix → Claude Code / OpenCode gateway via ACP (Agent Client Protocol).

## Prerequisites

- `claude-agent-acp`: `npm install -g @agentclientprotocol/claude-agent-acp`
- `opencode`: installed globally (optional, for `/agent opencode`)

## Commands

```bash
cargo build             # compile
cargo check             # type-check
cargo clippy            # lint
cargo fmt               # format
cargo test              # all tests (sequential via RUST_TEST_THREADS=1)
cargo run -- --config ~/.config/agent-gateway/config.toml    # --config is required
```

## Restart

```bash
pkill -x agent-gateway 2>/dev/null; sleep 1; nohup cargo run -- --config ~/.config/agent-gateway/config.toml > /tmp/gateway.log 2>&1 &
```

- `pkill -x agent-gateway` kills the binary by exact process name, NOT `cargo` wrapper.
- Do NOT use `pkill -f "cargo run"` — it may match the current command.
- Logs saved to `/tmp/gateway.log`.

## Global duplicate instance protection

Bot uses a shared account password. At startup it queries the device list and bails if another device named `agent-gateway` has `last_seen_ts < 60s`. This prevents two instances from running simultaneously with the same account. No mutual kick — the later instance simply exits with an error.

No account_data lock, no heartbeat, no password auth needed at startup.

## Architecture

- **`src/agent.rs`** — `AgentSession` trait (`send_input`, `async fn query_delta` returning `Option<AgentDelta>`, `id`), `AgentType` enum (`None`, `ClaudeCodeAcp`, `OpenCode`), `AgentRegistry` (manages both backends lazily)
- **`src/agent/cc.rs`** — `ClaudeCode`: spawns `claude-agent-acp`. `ClaudeCodeSession` holds `ActiveSession` directly, calls `send_prompt` / `read_update` on the ACP connection. `query_delta` has 1s timeout (no separate background task).
- **`src/agent/opencode.rs`** — `OpenCodeAgent`: same pattern but spawns `opencode acp`
- **`src/bot/matrix.rs`** — `MatrixBot`: event handlers, session management, streaming response loop.
- **`src/config.rs`** — TOML config parsing.
- **`src/main.rs`** — CLI args, dir resolution → `MatrixBot::new()` → `bot.run().await`

### Message flow

1. `on_room_message` → command `/...` paths are handled by `handle_command()` directly.
2. Plain text → `bot.run_user_prompt()` → `get_or_create_session()` → `run_task()`.
3. `get_or_create_session()`: takes `agent_session` from `Session.agent_session.take()` (reuse), or creates via `AgentRegistry::create_session()`. Stored back after response.
4. New room with no agent → returns error "No agent selected". User must `/agent claude-code-acp` or `/agent opencode` first. No auto-default.

### ACP flow

No background per-session actor:

1. `send_input(text)` → `session.send_prompt(text)` (sync)
2. `query_delta().await` → `session.read_update()` with 1s timeout, returns `Option<AgentDelta>`
3. `run_task()` polls `query_delta()` in a loop (1s timeout doubles as pacing, no separate sleep)

Permissions from the agent are auto-approved.

### Streaming responses

Bot uses Matrix `m.replace` edits:
1. Sends `*Thinking*` placeholder
2. `run_task` polls `query_delta().await`, accumulates output
3. Edits placeholder every 2s (or on completion)
4. Final edit removes `*Thinking*`

Fallback: if placeholder send fails, sends one-shot message.

### Room leave safety

`run_task` loop checks `room.typing_notice(true)` — 403 means no longer joined, triggers immediate break.

### Session lifecycle

- **Session file**: `room_sessions.json` in state dir, persists `agent_session_id` for cross-restart resume.
- **Cross-restart resume**: on next message, `session/load` is attempted first; falls back to `session/new`.
- **Kick/leave**: session entry removed from map (drops `agent_session`, closing the ACP session).
- **`/reset`**: drops current agent session, clears `agent_session_id`.
- **`/setpwd <path>`**: set working directory for the room's agent.

## Config

`~/.config/agent-gateway/config.toml`:
```toml
[matrix]
id = "@user:server"
password = "..."
allowed-user = ["@admin:server"]
```

Data directories (XDG-style):

| Var | Default |
|---|---|
| `STATE_DIRECTORY` | `~/.local/state/agent-gateway` |
| `CACHE_DIRECTORY` | `~/.cache/agent-gateway` |

## Important

- `README.md` is stale — it describes the old `claude -p` architecture, not the current ACP-based one. Do not rely on it.
- `--debug` enables debug-level logging (default: info).
- Tests run sequentially (`RUST_TEST_THREADS=1` in `.cargo/config.toml`).
