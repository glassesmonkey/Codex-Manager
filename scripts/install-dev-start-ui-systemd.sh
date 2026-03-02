#!/usr/bin/env bash
set -euo pipefail

SERVICE_UNIT="${SERVICE_UNIT:-codexmanager-dev-service.service}"
WEB_UNIT="${WEB_UNIT:-codexmanager-dev-web.service}"
LEGACY_UNIT="${LEGACY_UNIT:-codexmanager-dev-ui.service}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
START_SCRIPT="${ROOT_DIR}/scripts/dev-start-ui.sh"
SYSTEMD_USER_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user"
SERVICE_UNIT_PATH="${SYSTEMD_USER_DIR}/${SERVICE_UNIT}"
WEB_UNIT_PATH="${SYSTEMD_USER_DIR}/${WEB_UNIT}"
PATH_VALUE="${PATH}"
CARGO_BIN=""
PNPM_BIN=""

if ! command -v systemctl >/dev/null 2>&1; then
  echo "[install-systemd] error: systemctl not found. This script requires Linux systemd." >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "[install-systemd] error: cargo not found in PATH." >&2
  exit 1
fi

if ! command -v pnpm >/dev/null 2>&1; then
  echo "[install-systemd] error: pnpm not found in PATH." >&2
  exit 1
fi

CARGO_BIN="$(command -v cargo)"
PNPM_BIN="$(command -v pnpm)"

if [[ ! -x "${START_SCRIPT}" ]]; then
  echo "[install-systemd] error: start script is missing or not executable: ${START_SCRIPT}" >&2
  exit 1
fi

mkdir -p "${SYSTEMD_USER_DIR}"

cat > "${SERVICE_UNIT_PATH}" <<EOF
[Unit]
Description=CodexManager dev backend service
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
WorkingDirectory=${ROOT_DIR}
Environment=PATH=${PATH_VALUE}
ExecStart=${CARGO_BIN} run -p codexmanager-service
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
EOF

cat > "${WEB_UNIT_PATH}" <<EOF
[Unit]
Description=CodexManager dev web
After=network-online.target ${SERVICE_UNIT}
Wants=network-online.target ${SERVICE_UNIT}
StartLimitIntervalSec=0

[Service]
Type=simple
WorkingDirectory=${ROOT_DIR}
Environment=PATH=${PATH_VALUE}
Environment=CODEXMANAGER_WEB_ROOT=${ROOT_DIR}/apps/dist
Environment=CODEXMANAGER_WEB_NO_OPEN=1
Environment=CODEXMANAGER_WEB_NO_SPAWN_SERVICE=1
ExecStartPre=${PNPM_BIN} -C apps run build
ExecStart=${CARGO_BIN} run -p codexmanager-web
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
EOF

echo "[install-systemd] wrote unit: ${SERVICE_UNIT_PATH}"
echo "[install-systemd] wrote unit: ${WEB_UNIT_PATH}"

if command -v curl >/dev/null 2>&1; then
  echo "[install-systemd] stopping existing listeners on default ports..."
  curl --max-time 1 --silent "http://localhost:48761/__quit" >/dev/null 2>&1 || true
  curl --max-time 1 --silent "http://localhost:48760/__shutdown" >/dev/null 2>&1 || true
  sleep 1
fi

systemctl --user disable --now "${SERVICE_UNIT}" "${WEB_UNIT}" >/dev/null 2>&1 || true
systemctl --user daemon-reload
systemctl --user disable --now "${LEGACY_UNIT}" >/dev/null 2>&1 || true
rm -f "${SYSTEMD_USER_DIR}/${LEGACY_UNIT}"
systemctl --user enable --now "${SERVICE_UNIT}" "${WEB_UNIT}"

echo
echo "[install-systemd] enabled and started:"
echo "  - ${SERVICE_UNIT}"
echo "  - ${WEB_UNIT}"
echo "[install-systemd] status (${SERVICE_UNIT}):"
systemctl --user status "${SERVICE_UNIT}" --no-pager --lines=20 || true
echo
echo "[install-systemd] status (${WEB_UNIT}):"
systemctl --user status "${WEB_UNIT}" --no-pager --lines=20 || true
echo
echo "[install-systemd] common commands:"
echo "  systemctl --user restart ${SERVICE_UNIT} ${WEB_UNIT}"
echo "  systemctl --user stop ${SERVICE_UNIT} ${WEB_UNIT}"
echo "  systemctl --user disable ${SERVICE_UNIT} ${WEB_UNIT}"
echo "  journalctl --user -u ${SERVICE_UNIT} -f"
echo "  journalctl --user -u ${WEB_UNIT} -f"
echo
echo "[install-systemd] optional: run after logout/reboot without user login:"
echo "  sudo loginctl enable-linger ${USER}"
