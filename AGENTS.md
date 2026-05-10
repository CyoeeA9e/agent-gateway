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

- `pkill -x agent-gateway` kills the binary by exact process name (not `cargo` wrapper).
- Do NOT use `pkill -f "cargo run"` — it may match the current command.
- Logs saved to `/tmp/gateway.log`.
- Duplicate instance check: if another device named `agent-gateway` with activity <60s is detected, startup fails.

## Architecture

- **`src/agent.rs`** — `AgentSession` trait (`send_input`, `async fn query_delta` returning `Option<AgentDelta>`, `id`), `AgentType` enum (`None`, `ClaudeCodeAcp`, `OpenCode`), `AgentRegistry` (manages both backends lazily)
- **`src/agent/cc.rs`** — `ClaudeCode`: spawns `claude-agent-acp`. `ClaudeCodeSession` holds `ActiveSession` directly, calls `send_prompt` / `read_update` on the ACP connection. `query_delta` has 1s timeout (no separate background task).
- **`src/agent/opencode.rs`** — `OpenCodeAgent`: same pattern as `ClaudeCode` but spawns `opencode acp`
- **`src/bot/matrix.rs`** — `MatrixBot` wraps all state (`AsyncMutex<AgentRegistry>`, `AsyncMutex<HashMap<String, Session>>`, `allowed`, `bot_id`). Event handlers receive `&MatrixBot`, access fields directly. `Session` stores per-room state (pwd, agent_type, agent_session, agent_session_id for cross-restart resume).
- **`src/main.rs`** — Config → `MatrixBot::new()` → `bot.run().await`

### ACP flow

No background per-session actor:

1. `send_input(text)` → `session.send_prompt(text)` (sync)
2. `query_delta().await` → `session.read_update()` with 1s timeout, returns `Option<AgentDelta>`
3. `MatrixBot::run_task()` polls `query_delta()` — timeout doubles as pacing, no separate `sleep`

Permissions from the agent are auto-approved.

### Backend selection

Default: `AgentType::None` (message rejected with "select an agent").
Per-room via Matrix chat:
- `/agent` — show current agent
- `/agent claude-code-acp` — switch to Claude Code
- `/agent opencode` — switch to OpenCode

Backends are lazy-started on first use (both backends can coexist; chosen per-room).

### Session lifecycle

- **New room**: `Session` created with `agent_type: None`. User must `/agent <type>` first.
- **Message**: `MatrixBot::get_or_create_session()` takes agent from `Session.agent_session.take()`, or creates one via `AgentRegistry::create_session()`. Stored back after response.
- **Cross-restart resume**: `agent_session_id` persisted in `room_sessions.json`. On next message, `session/load` is attempted first; falls back to `session/new`.
- **Session/load history**: replay drained in `load_session()` — `AvailableCommandsUpdate` or `UsageUpdate` marks completion.
- **Kick/leave**: session entry removed from map (drops `agent_session`, closing the ACP session).
- **`/reset`**: drops current agent session, clears `agent_session_id`.

### Streaming responses

Bot uses Matrix `m.replace` edits:
1. Sends `*Thinking*` placeholder
2. `run_task` polls `query_delta().await` every ~1s, accumulates output
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

- `CLAUDE.md` is stale — do not rely on it
- `--debug` enables debug-level logging (default: info)
- Tests run sequentially (`RUST_TEST_THREADS=1` in `.cargo/config.toml`)
