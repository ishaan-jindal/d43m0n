# Poltergeist zsh integration (SRS FR-1..FR-8).
# Install: eval "$(poltergeist init zsh)"
#
# Contract with `poltergeist hook`:
#   poltergeist hook pre  --cmd <text> --cwd <path>
#   poltergeist hook post --exit <code> --duration-ms <ms>
#   * prints at most one whisper line on stdout, or nothing;
#   * always exits 0 and returns instantly when the daemon is down.
# Leading-space commands and the ignore list are sent as-is; the daemon filters.
# This file must never change $?, command output, or command execution.

zmodload zsh/datetime 2>/dev/null
autoload -Uz add-zsh-hook 2>/dev/null

: "${POLTERGEIST_GLYPH:=👻}"
: "${POLTERGEIST_HOOK_TIMEOUT:=0.04}"
__poltergeist_bin=${POLTERGEIST_BIN:-poltergeist}
__poltergeist_start=
__poltergeist_ran=

__poltergeist_emit() {
  [[ -n $1 ]] || return 0
  if [[ -z ${NO_COLOR+x} && -t 2 ]]; then
    print -u2 -r -- $'\e[2;3m'"${POLTERGEIST_GLYPH} $1"$'\e[0m'
  else
    print -u2 -r -- "${POLTERGEIST_GLYPH} $1"
  fi
  return 0
}

# Run the hook client under the hard timeout, print its whisper if any.
# Never propagates failure: a dead daemon or missing binary is silent.
__poltergeist_call() {
  local whisper
  if (( $+commands[timeout] )); then
    whisper=$(timeout "$POLTERGEIST_HOOK_TIMEOUT" "$@" 2>/dev/null) || return 0
  else
    whisper=$("$@" 2>/dev/null) || return 0
  fi
  __poltergeist_emit "$whisper"
}

__poltergeist_preexec() {
  local ret=$? cmd=$1
  __poltergeist_ran=1
  __poltergeist_call "$__poltergeist_bin" hook pre --cmd "$cmd" --cwd "$PWD"
  __poltergeist_start=$EPOCHREALTIME
  return $ret
}

__poltergeist_precmd() {
  local ret=$? start=$__poltergeist_start
  local -i elapsed_ms=0
  # precmd also runs for the first prompt, before any command ran: send nothing.
  [[ -n $__poltergeist_ran ]] || return $ret
  __poltergeist_ran=
  __poltergeist_start=
  if [[ -n $start && -n $EPOCHREALTIME ]]; then
    (( elapsed_ms = (EPOCHREALTIME - start) * 1000 ))
    (( elapsed_ms < 0 )) && elapsed_ms=0
  fi
  __poltergeist_call "$__poltergeist_bin" hook post --exit "$ret" --duration-ms "$elapsed_ms"
  return $ret
}

add-zsh-hook preexec __poltergeist_preexec 2>/dev/null
add-zsh-hook precmd __poltergeist_precmd 2>/dev/null
