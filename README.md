# agent-gateway

Matrix bot gateway for [Claude Code](https://claude.ai/code). Each Matrix room gets its own isolated Claude conversation with persistent context.

## Setup

### Prerequisites

- Rust toolchain (edition 2024)
- [Claude Code](https://claude.ai/code) CLI (`claude`) in PATH and authenticated

### Configuration

Create `gateway.toml`:

```toml
[matrix]
id = "@bot:example.com"
password = "your-matrix-password"
allowed-user = ["@alice:example.com", "@bob:example.com"]
```

### Install & Run

```bash
cargo install --path .
agent-gateway --install-systemd-user   # install systemd unit
agent-gateway --run-systemd            # start the service
```

Or run directly:

```bash
cargo run
```

### CLI

| Flag | Description |
|------|-------------|
| `--config <path>` | Config file path (default: `gateway.toml`) |
| `--print` | Print raw Claude stdout/stderr for debugging |
| `--install-systemd-user` | Install user systemd unit and exit |
| `--run-systemd` | Start via systemd |

## Commands

In a Matrix room, the bot responds to:

| Command | Description |
|---------|-------------|
| `/help` | Show available commands |
| `/setpwd` | Show current working directory for the room |
| `/setpwd <path>` | Set the working directory for Claude in this room |

Each room defaults to an isolated temp directory.

## Architecture

- `src/agent.rs` — `Agent` trait: `send_user_input` + `query_agent_delta`
- `src/agent/cc.rs` — `ClaudeCode`: spawns `claude -p --output-format stream-json --verbose`, parses JSON stream events
- `src/main.rs` — Matrix bot: invite handling, message routing, command dispatch, polling loop
- `src/room_sessions.rs` — Persistent `room_id → Session` mapping via JSON
- `src/session.rs` — `Session` per room: agent session ID, working directory
- `src/config.rs` — TOML config parsing

### Data directories

| Env var | Default | Purpose |
|---------|---------|---------|
| `STATE_DIRECTORY` / `XDG_STATE_HOME` | `~/.local/state/agent-gateway` | Matrix session, crypto store, room-sessions mapping |
| `CACHE_DIRECTORY` / `XDG_CACHE_HOME` | `~/.cache/agent-gateway` | Claude session data |

## Testing

```bash
cargo test                              # all tests (sequential)
cargo test test_single_turn             # single test
cargo test -- --nocapture               # show output
```

Integration tests cover single/multi-turn chat, tool use, session persistence, and encrypted Matrix round-trips.
