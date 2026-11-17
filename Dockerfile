# Notedown OCI image: the sync server, the static musl Linux build (no OCR).
#
# Base: distroless static (Debian 13), user nonroot (65532:65532). It ships CA
# certificates and nothing else: no shell, no package manager. Pinned by the
# digest of its multi-arch index (amd64 and arm64 inside); a change of the
# base moves the digest and the tag comment above FROM together.
#
# Build context layout (staged by the images job of the release before it
# calls buildx):
#   dist/linux-amd64/notedown
#   dist/linux-arm64/notedown
#   dist/doc/            LICENSE, NOTICE, README.md, THIRD-PARTY-NOTICES.txt
# Buildx sets TARGETARCH per platform (amd64, arm64), so the COPY below takes
# the matching binary. There is no RUN step, so a cross build needs no QEMU.
#
# No HEALTHCHECK: distroless has no shell or curl, and notedown has no
# healthcheck subcommand.

# gcr.io/distroless/static-debian13:nonroot
FROM gcr.io/distroless/static-debian13@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931

ARG TARGETARCH
# Owned by root and not writable by the server. --chmod because a binary that
# went through an artifact upload may have lost its exec bit.
COPY --chmod=0755 dist/linux-${TARGETARCH}/notedown /usr/local/bin/notedown
COPY dist/doc/ /usr/share/doc/notedown/

# The data folder (server.toml, accounts, logs). notedown reads NOTEDOWN_DATA
# when --data is not given, so `docker exec <container> notedown cloud ...`
# works on the same folder as the server.
ENV NOTEDOWN_DATA=/data
# The default listen address in server.toml is 0.0.0.0:19072.
EXPOSE 19072

USER nonroot

# Ship /data owned by the runtime user (65532:65532), so a fresh named volume
# mounted there is writable from the first start. Docker copies the image's
# content and ownership into a named volume on its first use, but VOLUME alone
# leaves no /data in the image, and a volume over a missing path is created
# owned by root, which nonroot cannot write and distroless cannot chown.
# WORKDIR creates a missing directory owned by the current USER, which is why
# it comes after USER. The second WORKDIR sets the working directory back to /.
# A bind mount keeps the host folder's ownership: it must be writable by uid
# 65532 on the host.
WORKDIR /data
WORKDIR /

# VOLUME comes after the WORKDIR lines: the legacy builder drops changes made
# to a path after it is declared a volume, BuildKit keeps them. In this order
# both builders ship /data owned by nonroot.
VOLUME /data

ENTRYPOINT ["/usr/local/bin/notedown"]
CMD ["serve"]
