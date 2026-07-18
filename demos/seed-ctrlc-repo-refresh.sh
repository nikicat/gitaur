# shellcheck shell=bash
# Seed for the ctrlc-repo-refresh demo (sourced inside the container after
# lib.sh, by demos/build.sh's record step AND by
# tests/container/extended/39_demo_ctrlc_repo_refresh.sh — one seed, two
# consumers).
#
# Make pacman.conf hermetic — only [options] + [local-repo] survive, same
# shape as smoke/55 — and point the surviving Server at the hung server the
# driver starts on 127.0.0.1:18791 (the port constant lives in
# examples/demo_ctrlc_repo_refresh.rs): the `refresh pacman` under demo then
# parks mid-download, which is the point. The repo sync itself stays off
# until the *driver* flips check_repo_updates, so the mirror-bootstrap
# `aurox -Sy` that runs between this seed and the driver never dials the
# hung server.
awk '/^\[/ { keep = ($0 == "[options]" || $0 == "[local-repo]") } keep' \
    /etc/pacman.conf > /tmp/pacman.conf.hermetic
sudo cp /tmp/pacman.conf.hermetic /etc/pacman.conf
sudo sed -i 's|^Server = .*|Server = http://127.0.0.1:18791|' /etc/pacman.conf
