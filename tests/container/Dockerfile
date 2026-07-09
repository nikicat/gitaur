# aurox integration test runner — ephemeral Arch userspace.
#
# Three things are baked into the image so per-test containers start ready:
#   1. base-devel + makepkg + fakeroot + rust toolchain
#   2. an unprivileged `builder` user with passwordless sudo (makepkg refuses root)
#   3. ALL fixture PKGBUILDs built and indexed — /srv/mock-aur (mock AUR git mirror)
#      and /srv/local-repo (local pacman sync DB) are populated and pacman.conf
#      is updated, with `pacman -Sy` already run.
#
# At runtime the aurox source tree is bind-mounted read-only at /work; the
# only per-test work is wiping ~/.local/state/aurox and writing config.toml.
# That keeps the inner-loop cost to "podman run + cargo-built binary exec".

FROM archlinux:latest

# Refresh keys + base-devel + tools needed for real makepkg runs.
# cargo-llvm-cov + llvm are present so this same image drives the coverage
# orchestrator (scripts/coverage.sh): instrumented build, cargo test, and the
# lcov merge/report step all happen inside the test image — the host (or CI
# runner) only needs podman.
RUN pacman -Syu --noconfirm --needed \
        base-devel \
        fakeroot \
        git \
        sudo \
        rust \
        cargo \
        cargo-llvm-cov \
        llvm \
        pacman-contrib \
    && pacman -Scc --noconfirm

# `builder` is the non-root user makepkg/aurox tests run as.
# Passwordless sudo is fine *inside an ephemeral container* — never copy
# this pattern outside the test image.
RUN useradd -m -G wheel -s /bin/bash builder \
    && echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder

RUN install -d -o builder -g builder \
        /home/builder/.local/state/aurox \
        /home/builder/.cache/aurox \
        /srv/local-repo \
        /srv/mock-aur \
        /srv/foreign-pkgs \
        /work \
    && chown -R builder:builder /home/builder

# Bake fixtures — runs makepkg for every `repo=official` PKGBUILD, publishes
# every AUR PKGBUILD into /srv/mock-aur as its own `refs/heads/<pkgbase>` branch.
COPY --chown=builder:builder fixtures/ /home/builder/fixtures/
COPY setup-fixtures.sh /usr/local/bin/setup-fixtures.sh
USER builder
RUN FIXTURES_DIR=/home/builder/fixtures bash /usr/local/bin/setup-fixtures.sh

# Wire the local sync repo into pacman.conf and refresh its DB so per-test
# containers don't need to call sudo for either step.
USER root
RUN printf '\n[local-repo]\nSigLevel = Optional TrustAll\nServer = file:///srv/local-repo\n' \
        >> /etc/pacman.conf \
    && pacman -Sy --noconfirm

USER builder
WORKDIR /work
# `RUST_LOG=off` mutes the console tracing layer by default — the suite's shell
# assertions grep `ui::*` (eprintln) output only, and the PTY-driven loop test
# can't tolerate stray WARN lines scrolling the picker off the screen. The
# unconditional file-log layer (`state_dir()/logs/aurox-*.log` at debug) still
# captures everything for post-mortem; an individual test can re-export
# `RUST_LOG=…` if it specifically wants tracing on stderr.
ENV CARGO_TARGET_DIR=/tmp/target \
    RUST_LOG=off

CMD ["bash"]
