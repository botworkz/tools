#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

cargo install --locked --path "${REPO_ROOT}/botspawn" --force

BIN_DIR="${HOME}/.local/bin"
mkdir -p "${BIN_DIR}"
ln -sf "$(command -v botspawn)" "${BIN_DIR}/botforge"

echo "Installed botspawn and linked botforge -> botspawn at ${BIN_DIR}/botforge"
