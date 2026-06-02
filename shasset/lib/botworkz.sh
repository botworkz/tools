#!/usr/bin/env bash
# lib/botworkz.sh — helpers for the botworkz/mcp sibling checkout.
set -euo pipefail

if [[ "${_BOTSPACE_BOTWORKZ_LIB_SOURCED:-0}" == "1" ]]; then
  return 0
fi
_BOTSPACE_BOTWORKZ_LIB_SOURCED=1

BOTWORKZ_MCP_DIR="$(realpath -m "${BOTWORKZ_MCP_DIR:-${REPO_ROOT}/../mcp}")"

botworkz_containers_dir() { echo "${BOTWORKZ_MCP_DIR}/containers"; }

ensure_botworkz_sibling() {
  if [[ ! -f "${BOTWORKZ_MCP_DIR}/Makefile" ]]; then
    die "botworkz/mcp sibling not found or incomplete at ${BOTWORKZ_MCP_DIR} (missing Makefile). Clone https://github.com/botworkz/mcp next to this repo or set BOTWORKZ_MCP_DIR."
  fi
  if [[ ! -f "$(botworkz_containers_dir)/Makefile" ]]; then
    die "botworkz/mcp sibling not found or incomplete at ${BOTWORKZ_MCP_DIR} (missing $(botworkz_containers_dir)/Makefile). Clone https://github.com/botworkz/mcp next to this repo or set BOTWORKZ_MCP_DIR."
  fi
}
