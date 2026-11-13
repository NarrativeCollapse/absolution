#!/bin/sh
# Runs a command next to one virtiofsd per shared directory, for
# `make qemu share=...` and `make ch share=...`.
#   virtiofs-run.sh --share <socket>.sock <tag>=<dir>[:ro] [--share ...] -- <command...>

set -eu

STARTUP_TIMEOUT=5 # seconds
TAB="$(printf '\t')"
NL='
'

die() {
  echo "virtiofs: $*" >&2
  exit 1
}

usage() {
  echo "usage: $0 --share <socket>.sock <tag>=<dir>[:ro] [--share ...] -- <command...>" >&2
  exit 2
}

# One line per share: socket, tag, directory and "ro" or "rw", tab separated.
# Neither field may contain whitespace, which validate_share enforces.
SHARES=""
while [ "${1:-}" = --share ]; do
  [ $# -ge 3 ] || usage
  entry="$3"
  case "${entry}" in
    *=*) ;;
    *) die "share '${entry}': expected <tag>=<dir>" ;;
  esac
  mode=rw
  case "${entry}" in
    *:ro) entry="${entry%:ro}" mode=ro ;;
  esac
  SHARES="${SHARES}$2${TAB}${entry%%=*}${TAB}${entry#*=}${TAB}${mode}${NL}"
  shift 3
done
[ -n "${SHARES}" ] && [ "${1:-}" = -- ] && [ $# -ge 2 ] || usage
shift # the "--"

# virtiofsd holds a lock on <socket>.pid for as long as it runs.
share_in_use() {
  [ -e "$1.pid" ] && ! flock -n "$1.pid" true
}

SEEN_TAGS=" "
validate_share() {
  sock="$1" tag="$2" dir="$3"
  [ -n "${tag}" ] || die "share '=${dir}': empty tag"
  case "${tag}" in
    *[!A-Za-z0-9._-]*)
      die "tag '${tag}': only letters, digits, '.', '_' and '-' are allowed (is '${tag}=${dir}' a directory with an '=' in it? use tag=dir)"
      ;;
  esac
  [ ${#tag} -le 36 ] || die "tag '${tag}': longer than 36 bytes"
  case "${SEEN_TAGS}" in
    *" ${tag} "*) die "tag '${tag}' is used twice" ;;
  esac
  SEEN_TAGS="${SEEN_TAGS}${tag} "
  [ -n "${dir}" ] || die "share '${tag}': empty directory"
  case "${dir}" in
    *[[:space:]]*) die "share '${tag}': directories containing whitespace are not supported ('${dir}')" ;;
  esac
  [ -d "${dir}" ] || die "share '${tag}': ${dir} is not a directory"
  case "${sock}" in
    *[[:space:]]*) die "socket '${sock}': paths containing whitespace are not supported" ;;
    *.sock) ;;
    *) die "socket '${sock}' must end in .sock" ;;
  esac
  [ ${#sock} -lt 108 ] \
    || die "socket path too long (${#sock} bytes): ${sock}; use a shorter share_sockdir"
  if share_in_use "${sock}"; then
    die "${sock} is in use by another virtiofsd. Stop that VM first, or use another share_sockdir."
  fi
}

while IFS="${TAB}" read -r sock tag dir mode; do
  [ -n "${sock}" ] && validate_share "${sock}" "${tag}" "${dir}"
done <<SHARES
${SHARES}
SHARES

find_virtiofsd() {
  for candidate in "${VIRTIOFSD:-}" virtiofsd /usr/libexec/virtiofsd /usr/lib/qemu/virtiofsd; do
    [ -n "${candidate}" ] && command -v "${candidate}" && return 0
  done
  return 1
}

VIRTIOFSD_BIN="$(find_virtiofsd)" \
  || die "virtiofsd not found. Install it (e.g. 'dnf install virtiofsd' or 'pacman -S virtiofsd'), run 'make virtiofsd-fetch' (x86_64 Linux hosts), or set VIRTIOFSD=/path/to/virtiofsd"

# One line per running virtiofsd: PID, socket and tag, tab separated.
STARTED=""
PIDS=""

cleanup() {
  # Keep the exit status of the command (or of the signal) as the script's.
  status=$?
  trap - EXIT INT TERM
  if [ -n "${PIDS}" ]; then
    # PIDS is a space-separated list of numbers; splitting it is intended.
    # shellcheck disable=SC2086
    kill ${PIDS} 2>/dev/null || true
    # shellcheck disable=SC2086
    wait ${PIDS} 2>/dev/null || true
  fi
  while IFS="${TAB}" read -r sock _; do
    [ -n "${sock}" ] && rm -f "${sock}" "${sock}.pid"
  done <<SHARES
${SHARES}
SHARES
  exit "${status}"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

SHARED_DIRS=" "
start_virtiofsd() {
  sock="$1" tag="$2" dir="$3" mode="$4"
  mkdir -p "$(dirname "${sock}")"
  dir="$(cd "${dir}" && pwd)"
  case "${SHARED_DIRS}" in
    *" ${dir} "*)
      echo "virtiofs: warning: ${dir} is shared more than once; the instances do not share caches" >&2
      ;;
  esac
  SHARED_DIRS="${SHARED_DIRS}${dir} "

  readonly_flag=""
  [ "${mode}" = ro ] && readonly_flag=--readonly
  # An empty readonly_flag must disappear rather than become an empty argument.
  # shellcheck disable=SC2086
  "${VIRTIOFSD_BIN}" \
    --socket-path="${sock}" \
    --shared-dir "${dir}" \
    --sandbox none \
    --log-level warn \
    --translate-uid="map:0:$(id -u):1" \
    --translate-gid="map:0:$(id -g):1" \
    ${readonly_flag} \
    </dev/null >"${sock%.sock}.log" 2>&1 &
  PIDS="${PIDS} $!"
  STARTED="${STARTED}$!${TAB}${sock}${TAB}${tag}${NL}"
}

start_failed() {
  log="${1%.sock}.log"
  tail -n 20 "${log}" >&2 || true
  die "virtiofsd for '$2' $3; see ${log}"
}

# POSIX sleep only takes whole seconds; poll faster where sleep allows it.
if sleep 0.1 2>/dev/null; then
  NAP=0.1 TRIES=$((STARTUP_TIMEOUT * 10))
else
  NAP=1 TRIES=${STARTUP_TIMEOUT}
fi

wait_for_sockets() {
  tries=0
  while IFS="${TAB}" read -r pid sock tag; do
    [ -n "${pid}" ] || continue
    until [ -S "${sock}" ]; do
      kill -0 "${pid}" 2>/dev/null || start_failed "${sock}" "${tag}" "exited"
      [ "${tries}" -lt "${TRIES}" ] \
        || start_failed "${sock}" "${tag}" "did not create ${sock} within ${STARTUP_TIMEOUT}s"
      tries=$((tries + 1))
      sleep "${NAP}"
    done
  done <<STARTED
${STARTED}
STARTED
}

while IFS="${TAB}" read -r sock tag dir mode; do
  [ -n "${sock}" ] && start_virtiofsd "${sock}" "${tag}" "${dir}" "${mode}"
done <<SHARES
${SHARES}
SHARES
wait_for_sockets
while IFS="${TAB}" read -r sock tag dir mode; do
  [ -n "${sock}" ] || continue
  label=""
  [ "${mode}" = ro ] && label=" (read-only)"
  echo "virtiofs: sharing '${tag}' = ${dir}${label} (log: ${sock%.sock}.log)" >&2
done <<SHARES
${SHARES}
SHARES

# Not exec'd, so cleanup runs when the command ends.
"$@"
