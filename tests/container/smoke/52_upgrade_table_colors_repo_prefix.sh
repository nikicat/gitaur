#!/usr/bin/env bash
# Repo-prefix coloring in the upgrade table.
#
# `ui::repo` tints each repo column the way yay does: the name is run through
# yay's `text.ColorHash` (djb2 % 6) and rendered bold in one of six ANSI
# colors. "aur" hashes to bold blue (ESC[34m). This pins that end-to-end
# through `-Qu`, which renders `upgrade_table` directly (read-only, no sudo,
# no picker).
#
# Scope: one representative color is enough at the e2e layer — it proves color
# reaches rendered CLI output and that `--color always` forces it onto a pipe
# (console otherwise strips escapes when stdout/stderr isn't a TTY). The full
# core/extra/multilib/aur → color mapping is pinned exhaustively in the unit
# test `ui::tests::repo_colors_match_yay_colorhash`, so we don't re-verify each
# hash here (a container per color would be slow and redundant).
#
# Upgrade seed mirrors smoke/44: a foreign-installed pkgname at 1.0 whose
# pkgbase ships 2.0 in the mock AUR, so the plan has exactly one AUR row.
source /work/tests/container/lib.sh
bootstrap; reset_state

gaur -Sy
install_foreign test-syu-split-foreign-cli
assert_pkg_installed test-syu-split-foreign-cli

esc=$'\033'
aur_colored="${esc}[34m${esc}[1maur"   # bold blue — the ColorHash of "aur"

# Negative control: default (Auto) with stderr redirected to a file is non-TTY
# → no color, prefix is the bare word.
gaur -Qu
assert_exit 0
assert_stderr_contains "aur"
if grep -qF -- "$aur_colored" "$LAST_STDERR"; then
    echo "unexpected ANSI color on repo prefix in Auto/non-TTY mode" >&2
    _dump >&2
    exit 1
fi

# Force color on via config — the same row must now carry the bold-blue prefix
# even though stderr is still a pipe.
echo 'color = "always"' >> "$CONFIG_DIR/config.toml"
gaur -Qu
assert_exit 0
grep -qF -- "$aur_colored" "$LAST_STDERR" || {
    echo "expected bold-blue 'aur' prefix (${esc}[34m${esc}[1maur) with color = always" >&2
    _dump >&2
    exit 1
}
