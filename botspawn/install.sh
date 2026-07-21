#!/usr/bin/env bash
set -euo pipefail

REPO="botworkz/tools"
BIN_NAME="botspawn"
BIN_DIR="${XDG_BIN_HOME:-${HOME}/.local/bin}"
INSTALL_PATH="${BIN_DIR}/${BIN_NAME}"
BLOCK_START="# >>> botspawn install >>>"
BLOCK_END="# <<< botspawn install <<<"
CHANGES=()

note() {
  printf '%s\n' "$*"
}

record_change() {
  CHANGES+=("$*")
}

detect_os() {
  case "$(uname -s)" in
    Linux) echo "linux" ;;
    Darwin) echo "darwin" ;;
    *)
      note "Unsupported OS: $(uname -s)"
      exit 1
      ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo "x86_64" ;;
    aarch64|arm64) echo "aarch64" ;;
    *)
      note "Unsupported architecture: $(uname -m)"
      exit 1
      ;;
  esac
}

resolve_release_json() {
  local endpoint="${1}"
  curl -fsSL "${endpoint}"
}

select_asset_url() {
  local release_json="${1}"
  local target="${2}"
  python - "$release_json" "$target" <<'PY'
import json
import pathlib
import sys

release = json.loads(pathlib.Path(sys.argv[1]).read_text())
target = sys.argv[2]
candidates = [
    f"botspawn-{target}.tar.gz",
    f"botspawn-{target}.tgz",
    f"botspawn-{target}",
]
assets = {a.get("name"): a.get("browser_download_url") for a in release.get("assets", [])}
for name in candidates:
    if name in assets and assets[name]:
        print(f"{name}\t{assets[name]}")
        break
PY
}

install_path_block() {
  local shell_name rc_file line
  shell_name="$(basename "${SHELL:-}")"
  case "${shell_name}" in
    zsh) rc_file="${HOME}/.zshrc"; line="export PATH=\"${BIN_DIR}:\$PATH\"" ;;
    bash) rc_file="${HOME}/.bashrc"; line="export PATH=\"${BIN_DIR}:\$PATH\"" ;;
    fish) rc_file="${HOME}/.config/fish/config.fish"; line="fish_add_path \"${BIN_DIR}\"" ;;
    *) rc_file="${HOME}/.profile"; line="export PATH=\"${BIN_DIR}:\$PATH\"" ;;
  esac

  if [[ ":$PATH:" == *":${BIN_DIR}:"* ]]; then
    note "PATH already includes ${BIN_DIR}; no rc updates needed."
    return
  fi

  mkdir -p "$(dirname "${rc_file}")"
  touch "${rc_file}"

  if grep -Fq "${BLOCK_START}" "${rc_file}"; then
    note "PATH block already present in ${rc_file}; no rc updates needed."
    return
  fi

  {
    printf '\n%s\n' "${BLOCK_START}"
    printf '%s\n' "${line}"
    printf '%s\n' "${BLOCK_END}"
  } >> "${rc_file}"
  record_change "Updated ${rc_file} with botspawn PATH block."
}

main() {
  local os arch target version endpoint tag release_json_file asset_info asset_name asset_url archive tmpdir extracted
  os="$(detect_os)"
  arch="$(detect_arch)"
  target="${os}-${arch}"
  version="${BOTSPAWN_VERSION:-latest}"

  if [[ "${version}" == "latest" ]]; then
    endpoint="https://api.github.com/repos/${REPO}/releases/latest"
  else
    tag="${version#v}"
    endpoint="https://api.github.com/repos/${REPO}/releases/tags/v${tag}"
  fi

  tmpdir="$(mktemp -d)"
  trap 'rm -rf "${tmpdir}"' EXIT

  release_json_file="${tmpdir}/release.json"
  resolve_release_json "${endpoint}" > "${release_json_file}" || {
    note "Could not fetch release metadata from ${endpoint}."
    note "Fallback: install from source manually:"
    note "  cargo install --locked --git https://github.com/${REPO}.git ${BIN_NAME}"
    exit 1
  }

  asset_info="$(select_asset_url "${release_json_file}" "${target}")"
  if [[ -z "${asset_info}" ]]; then
    note "No release artifact found for ${target}."
    note "Expected one of: botspawn-${target}.tar.gz, .tgz, or raw binary."
    note "Fallback: install from source manually:"
    note "  cargo install --locked --git https://github.com/${REPO}.git ${BIN_NAME}"
    exit 1
  fi

  asset_name="${asset_info%%$'\t'*}"
  asset_url="${asset_info#*$'\t'}"
  archive="${tmpdir}/${asset_name}"

  mkdir -p "${BIN_DIR}"
  curl -fsSL "${asset_url}" -o "${archive}"

  extracted="${tmpdir}/${BIN_NAME}"
  case "${asset_name}" in
    *.tar.gz|*.tgz)
      tar -xzf "${archive}" -C "${tmpdir}"
      extracted="$(find "${tmpdir}" -type f -name "${BIN_NAME}" | head -n1)"
      ;;
    *)
      extracted="${archive}"
      ;;
  esac

  if [[ -z "${extracted}" || ! -f "${extracted}" ]]; then
    note "Downloaded artifact did not contain ${BIN_NAME}."
    exit 1
  fi

  chmod +x "${extracted}"
  if [[ -f "${INSTALL_PATH}" ]] && cmp -s "${extracted}" "${INSTALL_PATH}"; then
    note "${INSTALL_PATH} is already up to date."
  else
    install -m 0755 "${extracted}" "${INSTALL_PATH}"
    record_change "Installed ${BIN_NAME} to ${INSTALL_PATH}."
  fi

  install_path_block

  if [[ "${#CHANGES[@]}" -eq 0 ]]; then
    note "No changes were necessary."
  else
    note "Applied changes:"
    for change in "${CHANGES[@]}"; do
      note "- ${change}"
    done
  fi

  note "Done. Run '${BIN_NAME} --help' to verify."
}

main "$@"
