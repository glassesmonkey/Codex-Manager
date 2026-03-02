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

exec cargo run -p codexmanager-start
