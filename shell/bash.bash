# d43m0n bash integration: PROMPT_COMMAND post-whispers only.
# Pre-whispers in bash are out of scope for v1 (bash-preexec is needed for them).
# Install: eval "$(d43m0n init bash)"
#
# Contract with `d43m0n hook`:
#   d43m0n hook post --exit <code> --duration-ms <ms>
#   * prints at most one whisper line on stdout, or nothing;
#   * always exits 0 and returns instantly when the daemon is down.
# Leading-space commands and the ignore list are sent as-is; the daemon filters.
# This file must never change $?, command output, or command execution.
# Note: it installs its own DEBUG trap for command timing (post-only bash mode).

: ${POLTERGEIST_GLYPH:=👻}
: ${POLTERGEIST_HOOK_TIMEOUT:=0.04}
__d43m0n_bin=${POLTERGEIST_BIN:-d43m0n}
__d43m0n_start=

# One fork at init: does this date support %3N (GNU)? Otherwise durations are 0.
if __d43m0n_probe=$(date +%s%3N 2>/dev/null) && [[ $__d43m0n_probe =~ ^[0-9]+$ ]]; then
  __d43m0n_date_ms=1
else
  __d43m0n_date_ms=0
fi
unset __d43m0n_probe

__d43m0n_emit() {
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
__d43m0n_call() {
  local whisper
  if command -v timeout >/dev/null 2>&1; then
    whisper=$(timeout "$POLTERGEIST_HOOK_TIMEOUT" "$@" 2>/dev/null) || return 0
  else
    whisper=$("$@" 2>/dev/null) || return 0
  fi
  __d43m0n_emit "$whisper"
}

# Set __d43m0n_now_ms. No subshell on bash >= 5 (EPOCHREALTIME).
__d43m0n_now() {
  if [[ -n ${EPOCHREALTIME:-} ]]; then
    local t=${EPOCHREALTIME//[.,]/}
    __d43m0n_now_ms=$(( 10#$t / 1000 ))
  elif (( __d43m0n_date_ms )); then
    __d43m0n_now_ms=$(date +%s%3N)
  else
    __d43m0n_now_ms=0
  fi
  return 0
}

# Timestamp the first simple command after a prompt. Skips the prompt hook itself.
__d43m0n_debug() {
  [[ $BASH_COMMAND == __d43m0n_prompt ]] && return 0
  [[ -n $__d43m0n_start ]] && return 0
  __d43m0n_now
  __d43m0n_start=$__d43m0n_now_ms
  return 0
}

__d43m0n_run_original() {
  local cmd
  for cmd in "${__d43m0n_orig_prompt[@]}"; do
    eval "$cmd"
  done
  return 0
}

__d43m0n_prompt() {
  local ret=$? start now duration_ms
  start=$__d43m0n_start
  __d43m0n_start=
  __d43m0n_run_original
  if [[ -n $start ]]; then
    __d43m0n_now
    now=$__d43m0n_now_ms
    duration_ms=$(( now - start ))
    (( duration_ms < 0 )) && duration_ms=0
    __d43m0n_call "$__d43m0n_bin" hook post --exit "$ret" --duration-ms "$duration_ms"
  fi
  return "$ret"
}

# Wrap the existing PROMPT_COMMAND (string or array) instead of clobbering it.
if [[ ${PROMPT_COMMAND:-} != __d43m0n_prompt ]]; then
  __d43m0n_orig_prompt=()
  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
    __d43m0n_orig_prompt=("${PROMPT_COMMAND[@]}")
  elif [[ -n ${PROMPT_COMMAND:-} ]]; then
    __d43m0n_orig_prompt=("$PROMPT_COMMAND")
  fi
  PROMPT_COMMAND=__d43m0n_prompt
fi

trap '__d43m0n_debug' DEBUG
