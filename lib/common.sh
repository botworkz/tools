#!/usr/bin/env bash
set -euo pipefail

if [[ "${_BOTWORK_COMMON_LIB_SOURCED:-0}" == "1" ]]; then
  return 0
fi
_BOTWORK_COMMON_LIB_SOURCED=1

COMMON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "${COMMON_DIR}/.." && pwd)}"
BUILD_DIR="${BUILD_DIR:-${REPO_ROOT}/build}"

if [[ -t 2 ]]; then
  COLOR_RED='\033[31m'
  COLOR_YELLOW='\033[33m'
  COLOR_GREEN='\033[32m'
  COLOR_RESET='\033[0m'
else
  COLOR_RED=''
  COLOR_YELLOW=''
  COLOR_GREEN=''
  COLOR_RESET=''
fi

log_info() { echo -e "${COLOR_GREEN}==>${COLOR_RESET} $*" >&2; }
log_warn() { echo -e "${COLOR_YELLOW}WARN:${COLOR_RESET} $*" >&2; }
log_error() { echo -e "${COLOR_RED}ERROR:${COLOR_RESET} $*" >&2; }

die() { log_error "$*"; exit 1; }

ensure_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

verify_sha256() {
  local file="$1"
  local expected="$2"
  local label="${3:-$1}"

  ensure_command sha256sum
  [[ -f "${file}" ]] || die "missing file for sha256 verification (${label}): ${file}"

  local actual
  actual="$(sha256sum "${file}" | awk '{print $1}')"
  if [[ "${actual}" != "${expected}" ]]; then
    die "sha256 mismatch for ${label}: expected ${expected}, got ${actual}"
  fi
}

default_private_key_path() {
  echo "${BUILD_DIR}/packer_ssh_key"
}

ensure_default_keypair() {
  local private_key
  private_key="$(default_private_key_path)"
  mkdir -p "${BUILD_DIR}"
  if [[ ! -f "${private_key}" ]]; then
    log_info "Generating ephemeral keypair at ${private_key}"
    ssh-keygen -t ed25519 -N '' -f "${private_key}" >/dev/null
  elif [[ ! -f "${private_key}.pub" ]]; then
    log_info "Regenerating missing public key at ${private_key}.pub"
    ssh-keygen -y -f "${private_key}" > "${private_key}.pub"
    chmod 0644 "${private_key}.pub"
  fi
}

public_key_from_private() {
  local private_key="$1"
  if [[ -f "${private_key}.pub" ]]; then
    cat "${private_key}.pub"
  else
    ssh-keygen -y -f "${private_key}"
  fi
}

pick_accelerator() {
  local requested="${1:-auto}"
  case "${requested}" in
    auto)
      if [[ -e /dev/kvm ]]; then
        echo "kvm"
      else
        echo "tcg"
      fi
      ;;
    kvm|tcg)
      echo "${requested}"
      ;;
    *)
      die "invalid accelerator: ${requested} (expected kvm|tcg|auto)"
      ;;
  esac
}

packer_accelerator() {
  case "${1}" in
    kvm) echo "kvm" ;;
    tcg) echo "none" ;;
    *) die "invalid accelerator for packer: $1" ;;
  esac
}

compose_service_for_accel() {
  case "${1}" in
    kvm) echo "tools-kvm" ;;
    tcg) echo "tools" ;;
    *) die "invalid accelerator for compose service: $1" ;;
  esac
}

repo_relative_path() {
  local path
  path="$(realpath -m "$1")"
  case "${path}" in
    "${REPO_ROOT}"/*)
      echo "${path#"${REPO_ROOT}/"}"
      ;;
    *)
      die "path must be inside repository for compose mounts: ${path}"
      ;;
  esac
}

running_inside_docker() {
  [[ "${BOTWORK_IN_DOCKER:-0}" == "1" ]]
}

should_use_compose_qemu() {
  if running_inside_docker; then
    return 1
  fi

  if [[ "${GITHUB_ACTIONS:-}" == "true" ]] && command -v qemu-system-x86_64 >/dev/null 2>&1; then
    return 1
  fi

  return 0
}
