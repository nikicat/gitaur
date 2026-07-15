#!/usr/bin/env bash
# `aur = false` in config.toml is pacman-only mode: no prompts, no nags, no
# mirror — search/info/install degrade to the official repos and say why.
source /work/tests/container/lib.sh
bootstrap; reset_state
echo 'aur = false' >> "$CONFIG_DIR/config.toml"

# -Sy: no consent prompt, no mirror; one note says what runs instead.
aurox -Sy
assert_exit 0
assert_stderr_contains "AUR disabled (aur = false in config.toml)"
assert_stderr_not_contains "clone the AUR mirror"
[[ ! -e "$STATE_DIR/aur" ]] || { echo "pacman-only -Sy must not clone" >&2; _dump >&2; exit 1; }

# -Ss: repo rows without the "sync the AUR" nudge — the mode is a standing
# choice, not a missing setup step.
aurox -Ss "^repo-base$"
assert_exit 0
assert_stdout_contains "local-repo/repo-base"
assert_stderr_not_contains "no AUR index"

# -Si: the repo block prints; a miss names the disabled mode, not a fix.
aurox -Si repo-base
assert_exit 0
assert_stdout_contains "Name            : repo-base"
aurox -Si test-trivial
assert_exit 1
assert_stderr_contains "AUR disabled: aur = false in config.toml"

# -S an AUR-only name: the failure explains the mode and never offers a clone.
aurox -S --noconfirm test-trivial
[[ "$LAST_EXIT" != "0" ]] || { echo "expected nonzero exit" >&2; _dump >&2; exit 1; }
assert_stderr_contains "aur = false"
assert_stderr_not_contains "may be in the AUR"
