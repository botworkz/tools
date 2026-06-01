#!/usr/bin/env bash
# lib/tools.sh — helpers for the botworkz/botwork sibling checkout.
set -euo pipefail

if [[ "${_BOTSPACE_TOOLS_LIB_SOURCED:-0}" == "1" ]]; then
  return 0
fi
_BOTSPACE_TOOLS_LIB_SOURCED=1

BOTWORK_TOOLS_DIR="$(realpath -m "${BOTWORK_TOOLS_DIR:-${REPO_ROOT}/../botwork}")"

ensure_tools_sibling() {
  if [[ ! -f "${BOTWORK_TOOLS_DIR}/Cargo.toml" ]]; then
    die "botworkz/botwork sibling not found or incomplete at ${BOTWORK_TOOLS_DIR} (missing Cargo.toml workspace root). Clone https://github.com/botworkz/botwork next to this repo or set BOTWORK_TOOLS_DIR."
  fi
}

build_tools_launcher() {
  ensure_command cargo
  log_info "Building botwork-launcher in ${BOTWORK_TOOLS_DIR} …"
  (
    cd "${BOTWORK_TOOLS_DIR}"
    cargo build --release --locked -p botwork-launcher
  )
}

build_tools_cli() {
  ensure_command cargo
  log_info "Building botwork-tools CLI in ${BOTWORK_TOOLS_DIR} …"
  (
    cd "${BOTWORK_TOOLS_DIR}"
    cargo build --release --locked -p botwork-tools
  )
}

fetch_tools_binaries() {
  ensure_command curl

  local version base_url
  version="${BOTWORK_TOOLS_IMAGES_VERSION:-latest}"

  if [[ "${version}" == "latest" ]]; then
    base_url="https://github.com/botworkz/botwork/releases/latest/download"
  else
    base_url="https://github.com/botworkz/botwork/releases/download/v${version}"
  fi

  mkdir -p "${BUILD_DIR}/bin"

  log_info "Downloading botwork-launcher from ${base_url} …"
  curl -fSL -o "${BUILD_DIR}/bin/botwork-launcher" "${base_url}/botwork-launcher"
  chmod +x "${BUILD_DIR}/bin/botwork-launcher"

  log_info "Downloading botwork-tools from ${base_url} …"
  curl -fSL -o "${BUILD_DIR}/bin/botwork-tools" "${base_url}/botwork-tools"
  chmod +x "${BUILD_DIR}/bin/botwork-tools"
}
