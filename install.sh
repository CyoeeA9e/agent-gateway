#!/usr/bin/env bash
set -euo pipefail

BINARY="agentbot"
BIN_DIR="${HOME}/.local/bin"
CONFIG_DIR="${HOME}/.config/agentbot"
SERVICE_DIR="${HOME}/.config/systemd/user"
SERVICE_NAME="agentbot.service"

echo "Building ${BINARY}..."
cargo build --release

mkdir -p "${BIN_DIR}" "${CONFIG_DIR}" "${SERVICE_DIR}"
cp "target/release/${BINARY}" "${BIN_DIR}/${BINARY}"

if [ ! -f "${CONFIG_DIR}/config.toml" ]; then
    cp config.toml.example "${CONFIG_DIR}/config.toml"
    echo "Created ${CONFIG_DIR}/config.toml from example — edit it with your JID and password."
fi

cat > "${SERVICE_DIR}/${SERVICE_NAME}" <<UNIT
[Unit]
Description=agentbot — XMPP bot forwarding to Claude Code
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${BIN_DIR}/${BINARY} -c ${CONFIG_DIR}/config.toml
StateDirectory=agentbot
CacheDirectory=agentbot
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
UNIT

systemctl --user daemon-reload
systemctl --user enable "${SERVICE_NAME}"
systemctl --user restart "${SERVICE_NAME}" 2>/dev/null || true

echo "Installed ${SERVICE_NAME} under systemd --user."
echo "  Binary:  ${BIN_DIR}/${BINARY}"
echo "  Config:  ${CONFIG_DIR}/config.toml"
echo "  Service: ${SERVICE_DIR}/${SERVICE_NAME}"
echo ""
echo "Status: systemctl --user status ${SERVICE_NAME}"
echo "Logs:   journalctl --user -u ${SERVICE_NAME} -f"
echo "Start:  systemctl --user start ${SERVICE_NAME}"
echo "Stop:   systemctl --user stop ${SERVICE_NAME}"
