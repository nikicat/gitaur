#!/usr/bin/env bash
# `-Ss <regex>` and `-Si <pkg>` against the AUR index.
source /work/tests/container/lib.sh
bootstrap; reset_state
aurox -Sy

aurox -Ss "^test-trivial$"
assert_exit 0
assert_stdout_contains "aur/test-trivial"

aurox -Si test-trivial
assert_exit 0
assert_stdout_contains "Name            : test-trivial"
# The index-sourced fields added for -Si parity…
assert_stdout_contains "Architecture    : any"
assert_stdout_contains "URL             : https://example.org/test-trivial"
# …and the live-sourced ones: the PKGBUILD's `# Maintainer:` comment and the
# fixture branch's git timestamps (one commit, so both dates exist).
assert_stdout_contains "Maintainer      : Trivial Upkeep <trivial@example.org>"
assert_stdout_contains "First Submitted : "
assert_stdout_contains "Last Updated    : "
