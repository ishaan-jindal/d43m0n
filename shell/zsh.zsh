# d43m0n zsh integration.
# Install: eval "$(d43m0n init zsh)"
#
# Contract with `d43m0n hook`:
#   d43m0n hook pre  --cmd <text> --cwd <path>
#   d43m0n hook post --exit <code> --duration-ms <ms>
#   * prints at most one whisper line on stdout, or nothing;
#   * always exits 0 and returns instantly when the daemon is down.
# Leading-space commands and the ignore list are sent as-is; the daemon filters.
# This file must never change $?, command output, or command execution.

zmodload zsh/datetime 2>/dev/null
autoload -Uz add-zsh-hook 2>/dev/null

: "${D43M0N_GLYPH:=👻}"
: "${D43M0N_HOOK_TIMEOUT:=0.04}"
__d43m0n_bin=${D43M0N_BIN:-d43m0n}
__d43m0n_start=
__d43m0n_ran=

__d43m0n_emit() {
  [[ -n $1 ]] || return 0
  if [[ -z ${NO_COLOR+x} && -t 2 ]]; then
    print -u2 -r -- $'\e[2;3m'"${D43M0N_GLYPH} $1"$'\e[0m'
  else
    print -u2 -r -- "${D43M0N_GLYPH} $1"
  fi
  return 0
}

# Run the hook client under the hard timeout, print its whisper if any.
# Never propagates failure: a dead daemon or missing binary is silent.
__d43m0n_call() {
  local whisper
  if (( $+commands[timeout] )); then
    whisper=$(timeout "$D43M0N_HOOK_TIMEOUT" "$@" 2>/dev/null) || return 0
  else
    whisper=$("$@" 2>/dev/null) || return 0
  fi
  __d43m0n_emit "$whisper"
}

__d43m0n_preexec() {
  local ret=$? cmd=$1
  __d43m0n_ran=1
  __d43m0n_call "$__d43m0n_bin" hook pre --cmd "$cmd" --cwd "$PWD"
  __d43m0n_start=$EPOCHREALTIME
  return $ret
}

__d43m0n_precmd() {
  local ret=$? start=$__d43m0n_start
  local -i elapsed_ms=0
  # precmd also runs for the first prompt, before any command ran: send nothing.
  [[ -n $__d43m0n_ran ]] || return $ret
  __d43m0n_ran=
  __d43m0n_start=
  if [[ -n $start && -n $EPOCHREALTIME ]]; then
    (( elapsed_ms = (EPOCHREALTIME - start) * 1000 ))
    (( elapsed_ms < 0 )) && elapsed_ms=0
  fi
  __d43m0n_call "$__d43m0n_bin" hook post --exit "$ret" --duration-ms "$elapsed_ms"
  return $ret
}

add-zsh-hook preexec __d43m0n_preexec 2>/dev/null
add-zsh-hook precmd __d43m0n_precmd 2>/dev/null
