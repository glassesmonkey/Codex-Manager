#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${ROOT_DIR}"

echo "[dev-start-ui] building frontend assets..."
pnpm -C apps run build

export CODEXMANAGER_WEB_ROOT="${ROOT_DIR}/apps/dist"
export CODEXMANAGER_WEB_NO_OPEN="${CODEXMANAGER_WEB_NO_OPEN:-1}"

echo "[dev-start-ui] starting service + web..."
echo "[dev-start-ui] web root: ${CODEXMANAGER_WEB_ROOT}"

# In fresh environments, cargo may be installed but not yet on PATH.
if ! command -v cargo >/dev/null 2>&1; then
  if [[ -f "${HOME}/.cargo/env" ]]; then
    # shellcheck source=/dev/null
    source "${HOME}/.cargo/env"
  fi
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "[dev-start-ui] error: cargo not found. Install Rust or add ~/.cargo/bin to PATH." >&2
  exit 1
fi

# codexmanager-start expects web/service binaries to exist alongside itself in target/debug.
if [[ ! -x "${ROOT_DIR}/target/debug/codexmanager-web" || ! -x "${ROOT_DIR}/target/debug/codexmanager-service" ]]; then
  echo "[dev-start-ui] building service/web binaries..."
  cargo build -p codexmanager-service -p codexmanager-web
fi

exec cargo run -p codexmanager-start
