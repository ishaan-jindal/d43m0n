# Poltergeist fish integration: preexec + postexec whispers.
# Install: poltergeist init fish | source
#
# Contract with `poltergeist hook`:
#   poltergeist hook pre  --cmd <text> --cwd <path>
#   poltergeist hook post --exit <code> --duration-ms <ms>
#   * prints at most one whisper line on stdout, or nothing;
#   * always exits 0 and returns instantly when the daemon is down.
# Leading-space commands and the ignore list are sent as-is; the daemon filters.
# This file must never change $status, command output, or command execution.

set -q POLTERGEIST_GLYPH; or set -g POLTERGEIST_GLYPH "👻"
set -q POLTERGEIST_HOOK_TIMEOUT; or set -g POLTERGEIST_HOOK_TIMEOUT 0.04
set -q __poltergeist_bin; or set -g __poltergeist_bin poltergeist
test -n "$POLTERGEIST_BIN"; and set -g __poltergeist_bin $POLTERGEIST_BIN

function __poltergeist_emit
    test -n "$argv"; or return 0
    if test -z "$NO_COLOR"; and isatty stderr
        printf '\e[2;3m%s %s\e[0m\n' "$POLTERGEIST_GLYPH" "$argv" >&2
    else
        printf '%s %s\n' "$POLTERGEIST_GLYPH" "$argv" >&2
    end
end

# Run the hook client under the hard timeout, print its whisper if any.
# Never propagates failure: a dead daemon or missing binary is silent.
function __poltergeist_call
    set -l whisper ""
    if command -q timeout
        set whisper (timeout $POLTERGEIST_HOOK_TIMEOUT $argv 2>/dev/null); or return 0
    else
        set whisper ($argv 2>/dev/null); or return 0
    end
    test -n "$whisper"; or return 0
    __poltergeist_emit "$whisper"
end

function __poltergeist_preexec --on-event fish_preexec
    set -l cmd $argv[1]
    __poltergeist_call $__poltergeist_bin hook pre --cmd "$cmd" --cwd "$PWD"
    # Timestamp after the pre-whisper so duration excludes hook latency.
    set -g __poltergeist_start (date +%s%3N 2>/dev/null); or set -e __poltergeist_start
end

function __poltergeist_postexec --on-event fish_postexec
    set -l last_status $status
    set -l cmd $argv[1]
    set -l duration_ms 0
    if set -q __poltergeist_start
        set -l now (date +%s%3N 2>/dev/null); or set -l now ""
        if string match -qr '^[0-9]+$' -- "$__poltergeist_start" "$now"
            set duration_ms (math "$now - $__poltergeist_start")
            test "$duration_ms" -ge 0; or set duration_ms 0
        end
        set -e __poltergeist_start
    else if set -q CMD_DURATION
        set duration_ms $CMD_DURATION
    end
    __poltergeist_call $__poltergeist_bin hook post --exit "$last_status" --duration-ms "$duration_ms"
    return $last_status
end
