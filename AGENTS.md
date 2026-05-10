# agent-gateway

Matrix → Claude Code gateway via ACP (Agent Client Protocol).

## Prerequisites

`claude-agent-acp` must be installed globally: `npm install -g @agentclientprotocol/claude-agent-acp`

Spawns a single persistent `claude-agent-acp` subprocess. All rooms share one child process but each gets its own ACP session.

## Commands

```bash
cargo build             # compile
cargo check             # type-check
cargo clippy            # lint
cargo fmt               # format
cargo test              # all tests (sequential via .cargo/config.toml)
cargo test --test simulate test_single_turn -- --nocapture  # single test with output
```

Tests run sequentially (`RUST_TEST_THREADS=1` in `.cargo/config.toml`). Start the gateway with:

```bash
cargo run -- --config ~/.config/agent-gateway/config.toml
```

## Restart after code changes

```bash
pkill -x agent-gateway 2>/dev/null; sleep 1; nohup cargo run -- --config ~/.config/agent-gateway/config.toml > /tmp/gateway.log 2>&1 &
```

This sequence is tested as the most reliable:
1. `pkill -x agent-gateway` — kills the running binary by exact process name (does not match `cargo` wrapper or zombie processes)
2. `sleep 1` — gives time for cleanup (session persist, ACP shutdown)
3. `nohup cargo run ... &` — builds and starts a new instance in background, logs saved to `/tmp/gateway.log`

Note: `pkill -x agent-gateway` matches only the binary process name, not the `cargo run` wrapper. Do NOT use `pkill -f "cargo run"` as it may match the current command. If the binary was started via `cargo run`, killing it will cause cargo to exit on its own.

The gateway checks for duplicate instances on startup via Matrix device listing — if another device with display name `agent-gateway` with recent activity (<60s) is detected, it exits with an error to prevent duplicate responses.

## Architecture

- **`src/agent.rs`** — `AgentSession` trait (`send_input`, `query_delta` — synchronous, object-safe) and `AgentRegistry` (wraps `ClaudeCode`, entry point for creating sessions). `AgentDelta` is an enum: `AgentDelta::Text { output, done }` and `AgentDelta::ToolCall { title }` (unused, reserved)
- **`src/agent/cc.rs`** — `ClaudeCode`: spawns `claude-agent-acp`, communicates via JSON-RPC 2.0 over STDIO using `agent-client-protocol` crate. `ClaudeCodeSession`: per-room `Box<dyn AgentSession>` handle. Only `ContentBlock::Text` from `AgentMessageChunk` is forwarded; `ToolCall`, `ToolCallUpdate`, `AgentThoughtChunk` are discarded
- **`src/bot/matrix.rs`** — `MatrixBot` with four event handlers (`on_invite`, `on_member_change`, `on_room_message`, `on_encrypted_message`). `agents` map (`HashMap<String, Box<dyn AgentSession>>`) for per-room sessions
- **`src/main.rs`** — Boilerplate: config → start registry → start bot → sync loop → shutdown

### ACP Flow

1. `AgentRegistry::start()` → initialize handshake, no session created yet
2. `AgentRegistry::create_session(pwd)` → sends `session/new` via ACP, spawns a background task (`run_session`) per session that reads updates and forwards to a per-session delta channel
3. `AgentSession::send_input()` → sends prompt to the session actor via channel
4. `AgentSession::query_delta()` → returns `Vec<AgentDelta>` (non-blocking, drains all available deltas)

Permissions from the agent (tool approval requests) are auto-approved.

### Session lifecycle

- Invite handler: joins room → `AgentRegistry::create_session()` → stores `Box<dyn AgentSession>` in agents map
- Message handler: `remove()`s agent from map → uses directly (`send_input`+`query_delta`) → `insert()`s back
- Kick/leave handler: removes agent from map (drops it → closes channel → stops session actor) and removes room from persisted sessions
- On restart: stale ACP session IDs are cleared; new sessions created on first message

### Encryption

Rooms are end-to-end encrypted. Message flow:
1. Decrypted messages → `on_room_message` handles directly
2. Encrypted messages (key not yet arrived) → `on_encrypted_message` queues event_id → `room_keys_received_stream` listener picks up keys → calls `room.event()` to fetch decrypted content → processes via Claude

### Streaming responses

Bot uses Matrix `m.replace` edits for streaming:
1. Sends `*Thinking*` placeholder message immediately
2. Polls Claude Code every 1s, accumulates output
3. Every 2s (or on completion), edits the placeholder to `<content>\n*Thinking*`
4. On completion, final edit removes `*Thinking*`, leaving only the response

Fallback: if placeholder send fails, sends final response as one-shot message.

### Room leave safety

If the bot is kicked or leaves a room mid-response, `process_with_claude()` detects it at the top of its polling loop — `room.typing_notice(true)` will fail with 403 when no longer joined, causing an immediate break. This prevents repeated `M_FORBIDDEN` errors from typing indicator refresh after leaving.

## Config

`~/.config/agent-gateway/config.toml`:
```toml
[matrix]
id = "@user:server"
password = "..."
allowed-user = ["@admin:server"]
```

Data directories (XDG-style fallback):

| Var | Default |
|---|---|
| `STATE_DIRECTORY` | `~/.local/state/agent-gateway` |
| `CACHE_DIRECTORY` | `~/.cache/agent-gateway` |

## Important

- `CLAUDE.md` is stale — do not rely on it for architecture details
- `--debug` enables debug-level logging (default: info)
