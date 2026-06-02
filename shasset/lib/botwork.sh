#!/usr/bin/env bash
# lib/botwork.sh — helpers for the botworkz/mcp-extra sibling checkout.
set -euo pipefail

if [[ "${_BOTSPACE_BOTWORK_LIB_SOURCED:-0}" == "1" ]]; then
  return 0
fi
_BOTSPACE_BOTWORK_LIB_SOURCED=1

BOTWORK_MCP_EXTRA_DIR="$(realpath -m "${BOTWORK_MCP_EXTRA_DIR:-${REPO_ROOT}/../mcp-extra}")"

botwork_containers_dir() { echo "${BOTWORK_MCP_EXTRA_DIR}/containers"; }
botwork_mcp_extra_containers_dir() { botwork_containers_dir; }

ensure_botwork_sibling() {
  if [[ ! -f "${BOTWORK_MCP_EXTRA_DIR}/Makefile" ]]; then
    die "botworkz/mcp-extra sibling not found or incomplete at ${BOTWORK_MCP_EXTRA_DIR} (missing Makefile). Clone botworkz/mcp-extra next to this repo or set BOTWORK_MCP_EXTRA_DIR."
  fi
  if [[ ! -f "$(botwork_containers_dir)/Makefile" ]]; then
    die "botworkz/mcp-extra sibling not found or incomplete at ${BOTWORK_MCP_EXTRA_DIR} (missing $(botwork_containers_dir)/Makefile). Clone botworkz/mcp-extra next to this repo or set BOTWORK_MCP_EXTRA_DIR."
  fi
}

ensure_botwork_mcp_extra_sibling() { ensure_botwork_sibling; }
