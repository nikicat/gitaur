#!/usr/bin/env bash
# The Ctrl-C-at-the-prompt demo driver, run as a plain test so the recorded
# flow can't rot. Ctrl-C at the *idle* shell prompt exits aurox with 130
# (128+SIGINT) — the prompt half of the Ctrl-C contract. Mid-operation a ^C
# bails back to the prompt (extended/31: builds; extended/37: AUR refresh;
# extended/39: repo refresh); at an idle prompt there is nothing to abort, so
# it means "leave the shell", like Ctrl-D but with an exit code a wrapper can
# tell apart from `quit`'s 0. The driver runs aurox from a real bash prompt
# and pins the code on screen via `echo $?`.
source /work/tests/container/lib.sh
bootstrap; reset_state

# Bootstrap the mirror + index so the shell opens at its normal prompt instead
# of the first-launch consent question.
aurox -Sy
assert_exit 0

driver="$EXAMPLES_DIR/demo_ctrlc_quit"
[[ -x "$driver" ]] || { echo "missing driver example: $driver (run.sh must build it)" >&2; exit 1; }

out="$(mktemp)"
if ! AUROX="$AUROX" "$driver" >"$out" 2>&1; then
    echo "ctrl-c prompt-quit demo driver failed (banner / ^C / echo \$?)" >&2
    cat "$out" >&2
    exit 1
fi
grep -qF 'DEMO_CTRLC_QUIT_OK' "$out" || { echo "driver did not report success" >&2; cat "$out" >&2; exit 1; }

echo "OK — idle-prompt ^C left the shell with exit 130"
