# agent-gateway

Matrix ↔ Claude Code / OpenCode gateway via [ACP (Agent Client Protocol)](https://github.com/agentclientprotocol).

Each Matrix room gets its own isolated agent session with persistent context. Sessions survive gateway restarts via ACP `session/resume`.

## Prerequisites

- Rust toolchain (edition 2024)
- `claude-agent-acp`: `npm install -g @agentclientprotocol/claude-agent-acp`
- `opencode` (optional, for `/agent opencode`)

## Configuration

Create `~/.config/agent-gateway/config.toml`:

```toml
[matrix]
id = "@bot:server"
password = "your_password"
allowed-user = ["@admin:server"]
```

See `utils/config.toml` for a commented example.

## Usage

```bash
cargo run
```

| Flag | Description |
|------|-------------|
| `--config <path>` | Config file path (default: `~/.config/agent-gateway/config.toml`) |
| `--debug` | Enable debug-level logging |
| `--install-user-service` | Install systemd user service and exit |

### Room commands

| Command | Description |
|---------|-------------|
| `/help` | Show available commands |
| `/agent <type>` | Switch agent (`none`, `claude-code-acp`, `opencode`) |
| `/reset` | Reset the current agent session |
| `/setpwd` | Show current working directory |
| `/setpwd <path>` | Set the working directory for the agent |

New rooms default to a temp directory and require `/agent` before any prompts.

### systemd service

```bash
cargo run -- --install-user-service
systemctl --user enable --now agent-gateway
```

The service unit is generated from `utils/agent-gateway.service` with the binary path and config path baked in.

## Architecture

- `src/bot/matrix.rs` — Matrix bot: invites, encrypted/plain messages, session management, commands
- `src/agent.rs` — `AgentSession` trait, `AgentType` enum, `AgentRegistry` (shared backends)
- `src/agent/cc.rs` — `ClaudeCode`: spawns `claude-agent-acp`, ACP tokio transport
- `src/agent/opencode.rs` — `OpenCodeAgent`: spawns `opencode acp`, ACP tokio transport
- `src/config.rs` — TOML config parsing
- `src/main.rs` — CLI entrypoint, dir resolution, bot startup

Messages flow: `on_room_message` → `handle_command()` (if starts with `/`) or `run_user_prompt()` → `get_or_create_session()` → `run_task()` (poll ACP delta loop, send full output on completion).

Permissions are auto-approved. Sessions persisted in `room_sessions.json` for cross-restart resume.

## Data directories

| Var | Default | Purpose |
|-----|---------|---------|
| `STATE_DIRECTORY` | `~/.local/state/agent-gateway` | Matrix session, crypto store, room sessions |
| `CACHE_DIRECTORY` | `~/.cache/agent-gateway` | Agent cache data |
