# Poltergeist bash integration (SRS FR-4): PROMPT_COMMAND post-whispers only.
# Pre-whispers in bash are out of scope for v1 (bash-preexec is needed for them).
# Install: eval "$(poltergeist init bash)"
#
# Contract with `poltergeist hook`:
#   poltergeist hook post --exit <code> --duration-ms <ms>
#   * prints at most one whisper line on stdout, or nothing;
#   * always exits 0 and returns instantly when the daemon is down.
# Leading-space commands and the ignore list are sent as-is; the daemon filters.
# This file must never change $?, command output, or command execution.
# Note: it installs its own DEBUG trap for command timing (post-only bash mode).

: ${POLTERGEIST_GLYPH:=👻}
: ${POLTERGEIST_HOOK_TIMEOUT:=0.04}
__poltergeist_bin=${POLTERGEIST_BIN:-poltergeist}
__poltergeist_start=

# One fork at init: does this date support %3N (GNU)? Otherwise durations are 0.
if __poltergeist_probe=$(date +%s%3N 2>/dev/null) && [[ $__poltergeist_probe =~ ^[0-9]+$ ]]; then
  __poltergeist_date_ms=1
else
  __poltergeist_date_ms=0
fi
unset __poltergeist_probe

__poltergeist_emit() {
  [[ -n $1 ]] || return 0
  if [[ -z ${NO_COLOR+x} && -t 2 ]]; then
    printf '\033[2;3m%s %s\033[0m\n' "$POLTERGEIST_GLYPH" "$1" >&2
  else
    printf '%s %s\n' "$POLTERGEIST_GLYPH" "$1" >&2
  fi
  return 0
}

# Run the hook client under the hard timeout, print its whisper if any.
# Never propagates failure: a dead daemon or missing binary is silent.
__poltergeist_call() {
  local whisper
  if command -v timeout >/dev/null 2>&1; then
    whisper=$(timeout "$POLTERGEIST_HOOK_TIMEOUT" "$@" 2>/dev/null) || return 0
  else
    whisper=$("$@" 2>/dev/null) || return 0
  fi
  __poltergeist_emit "$whisper"
}

# Set __poltergeist_now_ms. No subshell on bash >= 5 (EPOCHREALTIME).
__poltergeist_now() {
  if [[ -n ${EPOCHREALTIME:-} ]]; then
    local t=${EPOCHREALTIME//[.,]/}
    __poltergeist_now_ms=$(( 10#$t / 1000 ))
  elif (( __poltergeist_date_ms )); then
    __poltergeist_now_ms=$(date +%s%3N)
  else
    __poltergeist_now_ms=0
  fi
  return 0
}

# Timestamp the first simple command after a prompt. Skips the prompt hook itself.
__poltergeist_debug() {
  [[ $BASH_COMMAND == __poltergeist_prompt ]] && return 0
  [[ -n $__poltergeist_start ]] && return 0
  __poltergeist_now
  __poltergeist_start=$__poltergeist_now_ms
  return 0
}

__poltergeist_run_original() {
  local cmd
  for cmd in "${__poltergeist_orig_prompt[@]}"; do
    eval "$cmd"
  done
  return 0
}

__poltergeist_prompt() {
  local ret=$? start now duration_ms
  start=$__poltergeist_start
  __poltergeist_start=
  __poltergeist_run_original
  if [[ -n $start ]]; then
    __poltergeist_now
    now=$__poltergeist_now_ms
    duration_ms=$(( now - start ))
    (( duration_ms < 0 )) && duration_ms=0
    __poltergeist_call "$__poltergeist_bin" hook post --exit "$ret" --duration-ms "$duration_ms"
  fi
  return "$ret"
}

# Wrap the existing PROMPT_COMMAND (string or array) instead of clobbering it.
if [[ ${PROMPT_COMMAND:-} != __poltergeist_prompt ]]; then
  __poltergeist_orig_prompt=()
  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
    __poltergeist_orig_prompt=("${PROMPT_COMMAND[@]}")
  elif [[ -n ${PROMPT_COMMAND:-} ]]; then
    __poltergeist_orig_prompt=("$PROMPT_COMMAND")
  fi
  PROMPT_COMMAND=__poltergeist_prompt
fi

trap '__poltergeist_debug' DEBUG
