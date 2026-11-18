# The rlord-relay image: the static binary from a release tarball, on
# scratch, as a non-root user. Nothing is compiled here: the build context
# holds the Dockerfile and the release's Linux relay tarballs
# (rlord-relay-v<ver>-<arch>-unknown-linux-musl.tar.gz, one version), and
# each platform takes its own. `cargo xtask relay-image` stages that
# context from a release and builds linux/amd64 and linux/arm64:
#
#   docker buildx build --platform linux/amd64,linux/arm64 -t rlord-relay .
#
# Run it with the config mounted and a volume for the key and database.
# It serves what the config's `listen` says; inside a container that must
# not be loopback, so pass --listen (or set `listen` in the config):
#
#   docker run -v ./relay.toml:/etc/rlord-relay/relay.toml:ro \
#     -v rlord-relay:/var/lib/rlord-relay -p 127.0.0.1:8080:8080 \
#     rlord-relay serve --config /etc/rlord-relay/relay.toml --listen 0.0.0.0:8080
#
# compose.yml next to this file runs it behind Caddy.

# Only unpacks, so it runs on the build machine's own platform for every
# target (no emulation). The data dir is made as `check` would make it
# (0700): a new named volume takes its owner and mode from the image.
FROM --platform=$BUILDPLATFORM busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec AS unpack
ARG TARGETARCH
COPY rlord-relay-*.tar.gz /tarballs/
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) arch=x86_64 ;; \
      arm64) arch=aarch64 ;; \
      *) echo "no rlord-relay build for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    set -- /tarballs/rlord-relay-v*-"$arch"-unknown-linux-musl.tar.gz; \
    if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then \
      echo "want one $arch relay tarball in the build context, found: $*" >&2; exit 1; \
    fi; \
    mkdir /x && tar -xzf "$1" -C /x; \
    mkdir -p /rootfs/usr/share/doc/rlord-relay /rootfs/tmp /state/rlord-relay; \
    cp /x/*/rlord-relay /rootfs/rlord-relay; \
    cp /x/*/LICENSE-MIT /x/*/LICENSE-APACHE /rootfs/usr/share/doc/rlord-relay/; \
    chmod 0755 /rootfs/rlord-relay; \
    chmod 1777 /rootfs/tmp; \
    chmod 0700 /state/rlord-relay

FROM scratch
# Whole trees, so each folder in them keeps its mode (a COPY's own
# destination folder gets 0755).
COPY --from=unpack /rootfs/ /
COPY --from=unpack --chown=65532:65532 /state/ /var/lib/
LABEL org.opencontainers.image.title="rlord-relay" \
      org.opencontainers.image.description="The relay rlord hosts and devices dial out to. It sees only ciphertext." \
      org.opencontainers.image.source="https://github.com/robot-lord/robot-overlord" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
USER 65532:65532
VOLUME /var/lib/rlord-relay
EXPOSE 8080
ENTRYPOINT ["/rlord-relay"]
CMD ["serve", "--config", "/etc/rlord-relay/relay.toml"]
