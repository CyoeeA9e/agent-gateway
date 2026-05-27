# agentbot

XMPP bot that forwards messages to Claude Code via ACP subprocess (`claude-agent-acp`).

## Build & Run

```bash
cargo build --release
RUST_LOG=info ./target/release/agentbot                   # reads config.toml from cwd
RUST_LOG=info ./target/release/agentbot -c other.toml     # custom config path
```

Uses `tracing` (respects `RUST_LOG`). Exits on config parse failure.

## Config

`config.toml` — see `config.toml.example`.

```toml
[xmpp]
jid = "bot@example.com"
password = "your_password"
nick = "agentbot"             # optional, defaults to JID node, then "bot"
rooms = ["room@con.example"]  # optional, defaults to empty
```

## Architecture

```
main.rs
  tokio::select! {
    bot.listen_msg() → (XmppRequest, Option<Arc<dyn AgentSession>>)
      ├─ wait_for_events (200ms timeout)
      ├─ on ChatMessage / RoomMessage(with mention): get_or_resume_session()
      └─ returns request + optional session

    if bot.handle_command(&req)     → command::try_handle()   (/bot prefix)
    else spawn handle_request(req, maybe_session)
      ├─ no session  → "Echo: {msg}"
      └─ has session → send_input → poll query_delta in loop → resp when done
  }
```

**Command priority**: `/bot` commands are checked first. If the message starts with `/bot`, it is handled as a command and never reaches the agent.

## Message flow

1. `listen_msg()` loops on XMPP events with a 200ms timeout on `wait_for_events` (prevents deadlock — see below).
2. On `ChatMessage` or `RoomMessage` (with `@nick` / `nick:` / `nick,` / `nick ` mention, case-insensitive fallback), builds an `XmppRequest` and calls `get_or_resume_session()`.
3. `get_or_resume_session()`:
   - Checks in-memory `HashMap<String, Arc<dyn AgentSession>>` first.
   - Falls back to `state/sessions.json`. If `session_id` is non-empty → `resume_session()` via ACP. If empty but `agent` is set → `create_session()` (lazy). Otherwise → `None`.
   - On resume failure: logs warning, removes the stored entry, returns `None`.
 4. `handle_request()`: sets XMPP chat state to Composing. If no session, echoes the message back. If session, sends input to agent, then polls `query_delta()` in a loop:
    - `AgentDelta::Text` → accumulates output, sends on `done`.
    - `AgentDelta::ToolCall` → flushes accumulated text, then sends tool call info formatted as `ToolName(key="value", key="value")` (see ToolCall display).
    - `None` → retries (1s timeout on `read_update`).
    - On error → sends error message.

## Commands

Clap-based, prefix `/bot`. `/bot` alone shows help.

| Command | Behavior |
|---------|----------|
| `/bot help` | Show available commands |
| `/bot reset` | Prints a reset message but does **not** clear the stored session (no-op) |
| `/bot new <agent>` | Stores agent name in sessions.json; ACP session is lazy-created on next real message |
| `/bot pwd <path>` | Sets working directory for future sessions (canonicalized, must be a directory) |

Unknown commands show a parse error and suggest `/bot help`.

Agent dispatch is not implemented — `get_or_resume_session()` always calls `claude::create_session` regardless of the stored agent name. The `agent` field in sessions.json functions as a boolean flag (empty = no agent configured, non-empty = agent configured).

## Session lifecycle

- **In-memory**: `XmppBot.sessions: HashMap<String, Arc<dyn AgentSession>>` — keyed by conversation JID (bare JID string). Populated lazily on first message, or via `Bot::new_session()`.
- **On-disk**: `state/sessions.json` — `{ "<jid>": { "session_id", "agent", "pwd" } }`. Written by `set_agent`, `set_pwd`, `new_session`, and `get_or_resume_session` (on lazy create).
- **Lazy creation**: `/bot new <agent>` + `/bot pwd <path>` only write to sessions.json. The actual ACP session is created when the first real (non-command) message arrives.
- **Resume on message**: on each message, `get_or_resume_session()` attempts to resume a stored session from disk. Failing that, creates a new one if an agent is configured.
- **Singleton backend**: `claude-agent-acp` subprocess is spawned once via `OnceCell<ClaudeCodeAcp>` and shared across all sessions.
- **Permission auto-approval**: all tool-call permission requests are auto-approved (first option selected). Logged at info level.
- **State directory**: defaults to `$STATE_DIRECTORY` or `$STATE_DIR` env var, falling back to `state/`. Created if missing.

## XMPP lock scheme

The XMPP client (`xmpp::Agent`) is wrapped in `Arc<Mutex<_>>` and shared between:
- `listen_msg()` which holds the lock during `wait_for_events`
- `resp()` / `set_status()` called from spawned tokio tasks

To avoid deadlock, `listen_msg()` wraps `wait_for_events` in a **200ms timeout**. This periodically releases the lock so outbound messages can be sent. Without the timeout, an idle connection would starve all responses.

## ToolCall display

ACP sends tool calls as two separate notifications: `ToolCall` (title only, empty input) then `ToolCallUpdate` (command details with key-value input). `AcpSession` merges them via a `pending_tool_title` field:

1. `SessionUpdate::ToolCall` — stores `tc.title` (tool category, e.g. "Terminal", "Read File") in `pending_tool_title`. No delta produced.
2. `SessionUpdate::ToolCallUpdate` — takes the stored title, combines with `format_tool_input(&raw_input)`, produces a single `AgentDelta::ToolCall`.
3. If `ToolCallUpdate` arrives without a prior `ToolCall`, falls back to `tc.fields.title` as the tool name.

`format_tool_call_display(title, input)` in `agent.rs` converts the raw `key=value` pairs to `key="value"` (quoted values) and wraps in `Title(pairs)`.

Example user-facing output:
```
Terminal(command="ls -la /workspace", description="List files")
Read File(file_path="/workspace/Cargo.toml")
```

## Resp behavior

- `resp(text)` sends a raw message with the given text (no chat state payload).
- `set_status(Composing)` sends a raw message with empty body and `ChatState::Composing` payload.
- `set_status(Active)` sends a raw message with empty body and `ChatState::Active` payload.
- `handle_request()` sets Composing before agent interaction, Active after completion.

## Room mentions

In MUC rooms, only responds when the message is prefixed with `@nick`, `nick:`, `nick,`, or `nick ` (case-sensitive first, then ASCII case-insensitive fallback). Self-sent messages are ignored.

## Presence handling

- **Subscribe**: auto-replies with `subscribed` + `available` presence.
- **Unsubscribe/Unsubscribed**: auto-replies with `unsubscribed`.
- **Probe**: auto-replies with `available`.
- **Online**: sets presence to `Show::Chat` with status text, joins configured rooms.

## Test

```bash
python3 tests/buddy_system.py   # full E2E: commands, lazy session, resume, error handling
python3 test_xmpp_bot.py        # simple: testbot2 → testbot1 → expects "Echo: hello bot"
```

Both require `slixmpp`. Test accounts in `testbot.txt`. `.cargo/config.toml` sets `RUST_TEST_THREADS=1`.

## Additional files

- `buddy.c` — standalone buddy allocator demo (not part of agentbot; `gcc -Wall -Wextra -DBUDDY_DEMO -o buddy buddy.c`)
- `install.sh` — install script
