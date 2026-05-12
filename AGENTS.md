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
cargo run -- --config ~/.config/agent-gateway/config.toml    # --config is required
```

No tests exist in the codebase.

## Restart

```bash
pkill -x agent-gateway 2>/dev/null; sleep 1; nohup cargo run -- --config ~/.config/agent-gateway/config.toml > /tmp/gateway.log 2>&1 &
```

- `pkill -x agent-gateway` kills the binary by exact process name, NOT `cargo` wrapper.
- Do NOT use `pkill -f "cargo run"` — it may match the current command.
- Logs saved to `/tmp/gateway.log`.

## Global duplicate instance protection

At startup queries device list; bails if another device named `agent-gateway` has `last_seen_ts < 60s`. Prevents two instances running simultaneously with the same account.

## Architecture

- **`src/agent.rs`** — `AgentSession` trait, `AgentType` enum, `AgentRegistry` (manages both backends lazily)
- **`src/agent/cc.rs`** — `ClaudeCode`: spawns `claude-agent-acp`. `query_delta` has 1s timeout.
- **`src/agent/opencode.rs`** — `OpenCodeAgent`: same pattern but spawns `opencode acp`
- **`src/bot/matrix.rs`** — `MatrixBot`: all event handlers, session management, streaming response loop, commands.
- **`src/config.rs`** — TOML config parsing (`GatewayConfig`).
- **`src/main.rs`** — CLI args, dir resolution → `MatrixBot::new()` → `bot.run().await`

### Message flow

1. `on_room_message` → commands handled by `MatrixBot::handle_command()` (async method, locks sessions internally)
2. Plain text → `bot.run_user_prompt()` → `get_or_create_session()` → `run_task()`
3. `get_or_create_session()`: reuses `agent_session` from `Session.agent_session.take()` if available; otherwise `AgentRegistry::create_session()` which tries `session/resume` first (if `agent_session_id` exists), falls back to `session/new`
4. New room with no agent → error "No agent selected". User must `/agent claude-code-acp` or `/agent opencode` first. No default.

### Session lifecycle

- **Session file**: `room_sessions.json` in state dir, persists `agent_session_id` for cross-restart resume.
- **Cross-restart resume**: `session/resume` is attempted first; falls back to `session/new`.
- **`/setpwd <path>`**: updates `Session.pwd` and, if an idle agent session exists, calls `AgentRegistry::create_session(pwd, type, Some(sid))` → triggers `session/resume` with new cwd → backend detects fingerprint change and recreates the process.
- **Kick/leave**: session entry removed from map (drops `agent_session`, closing the ACP session).
- **`/reset`**: drops current agent session, clears `agent_session_id`.

### ACP flow

No background per-session actor:
1. `send_input(text)` → `session.send_prompt(text)` (sync)
2. `query_delta().await` → `session.read_update()` with 1s timeout
3. `run_task()` polls `query_delta()` in a loop (1s timeout doubles as pacing)
4. Permissions from the agent are auto-approved.

### Streaming responses

Bot uses Matrix `m.replace` edits:
1. Sends `*Thinking*` placeholder
2. `run_task` polls `query_delta()`, accumulates output
3. Edits placeholder every 2s (or on completion)
4. Final edit removes `*Thinking*`

Fallback: if placeholder send fails, sends one-shot message.

### Room leave safety

`run_task` loop checks `room.typing_notice(true)` — 403 means no longer joined, triggers immediate break.

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
- `MatrixError` is a public enum in the `bot::matrix` module (`Agent` / `Io` variants), used as `handle_command` return error type.
- `handle_command` returns `Result<Option<String>, MatrixError>`. `Ok(None)` = not a command; `Ok(Some(reply))` = handled; `Err(e)` = system error sent as `"Error: {e}"` to room.
