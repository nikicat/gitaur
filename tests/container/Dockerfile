# gitaur integration test runner — ephemeral Arch userspace.
#
# Three things are baked into the image so per-test containers start ready:
#   1. base-devel + makepkg + fakeroot + rust toolchain
#   2. an unprivileged `builder` user with passwordless sudo (makepkg refuses root)
#   3. ALL fixture PKGBUILDs built and indexed — /srv/mock-aur (mock AUR git mirror)
#      and /srv/local-repo (local pacman sync DB) are populated and pacman.conf
#      is updated, with `pacman -Sy` already run.
#
# At runtime the gitaur source tree is bind-mounted read-only at /work; the
# only per-test work is wiping ~/.local/state/gitaur and writing config.toml.
# That keeps the inner-loop cost to "podman run + cargo-built binary exec".

FROM archlinux:latest

# Refresh keys + base-devel + tools needed for real makepkg runs.
RUN pacman -Syu --noconfirm --needed \
        base-devel \
        fakeroot \
        git \
        sudo \
        rust \
        cargo \
        pacman-contrib \
    && pacman -Scc --noconfirm

# `builder` is the non-root user makepkg/gitaur tests run as.
# Passwordless sudo is fine *inside an ephemeral container* — never copy
# this pattern outside the test image.
RUN useradd -m -G wheel -s /bin/bash builder \
    && echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder

RUN install -d -o builder -g builder \
        /home/builder/.local/state/gitaur \
        /home/builder/.cache/gitaur \
        /srv/local-repo \
        /srv/mock-aur \
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
ENV CARGO_TARGET_DIR=/tmp/target \
    RUST_LOG=gitaur=info

CMD ["bash"]
