# seance.bash — shell integration for seance's default shell panes.
#
# Seance launches default shell panes as:
#     bash --init-file ~/.local/share/seance/seance.bash
# (the app copies this file there on startup). Because --init-file REPLACES the
# normal interactive startup, we first source the user's own bashrc so their
# shell is unchanged, then install command-boundary hooks so seance knows what
# ran in this pane: the command line, cwd, exit code, and duration. The hooks
# report over the control plane (`seance ctl cmd-begin` / `cmd-end`), which is
# fast, backgrounded, and silent — a shell here is a perfectly normal shell even
# when seance is unreachable.
#
# Everything below MUST degrade gracefully: if seance isn't on PATH or
# $SEANCE_SESSION isn't set (i.e. this shell wasn't spawned by seance), we
# install nothing and behave like a plain interactive bash.

# --- 1. Be a normal shell first. ------------------------------------------
# --init-file suppresses the usual ~/.bashrc; source it back so the user's
# aliases, prompt, completions, etc. all still apply. (No special guarding of
# the readline `bind` warning is needed — a normal interactive source is fine.)
if [ -f "$HOME/.bashrc" ]; then
    # shellcheck source=/dev/null
    . "$HOME/.bashrc"
fi

# --- 2. Only hook when we're actually a seance-spawned shell. --------------
# Gate on BOTH signals: the pane identity env var (so `from`/pane attribution
# works — ctl reads $SEANCE_SESSION itself) and `seance` being callable. If
# either is missing, install nothing and return: plain shell, zero overhead.
if [ -z "$SEANCE_SESSION" ] || ! command -v seance >/dev/null 2>&1; then
    return 0 2>/dev/null || true
fi

# --- 3. State for the hooks. -----------------------------------------------
# _seance_cmd holds the command line captured by the DEBUG trap for the command
# that's about to run. _seance_armed is the re-entry flag that makes the DEBUG
# trap capture exactly ONE command per prompt cycle (see the trap comment).
#
# Start DISARMED. The rest of THIS init file runs as ordinary commands that the
# DEBUG trap would otherwise capture (e.g. the PROMPT_COMMAND-setup `if` below).
# The first _seance_prompt (fired before the first interactive prompt) arms us,
# so the first thing we ever capture is a real user command.
_seance_cmd=""
_seance_armed=0

# --- 4. DEBUG trap: capture the command line BEFORE it runs. ---------------
# The DEBUG trap fires before *every* simple command — including each command
# inside PROMPT_COMMAND and inside compound statements — so we must guard hard
# against re-entry, or we'd log our own reporting call and every pipeline stage.
#
# Guards, in order:
#   * [ -n "$COMP_LINE" ] — we're in programmable completion, not a real command.
#   * "$BASH_COMMAND" == "$PROMPT_COMMAND" — the trap firing for the prompt hook
#     itself; skip it.
#   * _seance_armed — a one-shot latch: the trap captures the FIRST command after
#     a prompt, then disarms until the prompt hook re-arms it. This collapses a
#     pipeline / compound command to a single begin (its first-fired command's
#     text), instead of one begin per stage.
_seance_debug() {
    # In completion, or the prompt hook re-entering the trap → ignore.
    [ -n "$COMP_LINE" ] && return
    [ "$BASH_COMMAND" = "$PROMPT_COMMAND" ] && return
    # Already captured a command this cycle → ignore until the prompt re-arms.
    [ "$_seance_armed" = 1 ] || return

    _seance_cmd="$BASH_COMMAND"
    _seance_armed=0
    # Report the start, non-blocking. cwd travels explicitly; pane id travels via
    # $SEANCE_SESSION which ctl reads. Backgrounded + disowned + fully silenced so
    # a slow/absent control plane never stalls the interactive shell.
    seance ctl cmd-begin "$_seance_cmd" --cwd "$PWD" >/dev/null 2>&1 &
    disown 2>/dev/null || true
}
trap '_seance_debug' DEBUG

# --- 5. Prompt hook: report the exit code AFTER the command finished. ------
# PROMPT_COMMAND runs right before each prompt is drawn — i.e. after the previous
# command finished. Capture $? on the VERY FIRST line (before anything else can
# clobber it), report the end, then re-arm the DEBUG trap for the next command.
#
# We only report an end if the DEBUG trap actually captured a command this cycle
# (_seance_cmd non-empty). That skips the bare prompt at shell startup and any
# empty-line Enters, so we never emit a cmd-end without a matching cmd-begin.
_seance_prompt() {
    local exit=$?           # MUST be the first statement — captures the real $?.
    if [ -n "$_seance_cmd" ]; then
        seance ctl cmd-end "$exit" >/dev/null 2>&1 &
        disown 2>/dev/null || true
        _seance_cmd=""
    fi
    _seance_armed=1         # re-arm for the next command's DEBUG capture.
}

# Chain onto any PROMPT_COMMAND the user's bashrc already set, so we don't
# clobber their prompt logic. Bash 5.1+ allows PROMPT_COMMAND to be an array;
# handle both, appending ours last.
if [ "${BASH_VERSINFO[0]}" -gt 5 ] || { [ "${BASH_VERSINFO[0]}" = 5 ] && [ "${BASH_VERSINFO[1]}" -ge 1 ]; }; then
    # Array-capable bash: append as a new element (won't disturb existing ones).
    PROMPT_COMMAND+=("_seance_prompt")
else
    # Older bash: string form. Prepend with a separator so our $? capture runs
    # before any user prompt command mutates $? (we snapshot it first thing).
    if [ -n "$PROMPT_COMMAND" ]; then
        PROMPT_COMMAND="_seance_prompt;${PROMPT_COMMAND}"
    else
        PROMPT_COMMAND="_seance_prompt"
    fi
fi
