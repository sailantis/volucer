# The build prerequisites, spelled out rather than assumed: `rustls` avoids OpenSSL, so there is
# no dylib to match against a system version, but its default `aws-lc-rs` provider is C and
# assembly and needs a C toolchain, NASM and CMake. See the README's "Build prerequisites".
FROM rust:1.88-bookworm AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends nasm cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN cargo build --release --locked -p volucer


# A glibc image, which is the one that matches the host distribution's libc.
FROM debian:bookworm-slim AS glibc

# `ca-certificates` is not decoration: without it an ACME client cannot verify the CA's own HTTPS
# endpoint, and the failure reads as a network error rather than as a missing trust store.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/volucer /usr/local/bin/volucer

RUN useradd --system --create-home --home-dir /var/lib/volucer volucer
# The directory `CMD` names. Created here rather than left to the operator's bind mount, because a
# `serve` pointed at a path that does not exist reports a missing file, which reads as "the image is
# broken" rather than "you have not mounted your configuration yet".
RUN mkdir -p /etc/volucer
USER volucer

# `WORKDIR` creates the directory, and it is the one `--home-dir` already gave the user.
WORKDIR /var/lib/volucer

EXPOSE 8080 9080

# Two ports are exposed and only one of them is content: 8080 is `server.http` in the example
# configuration and 9080 is `[status]`, which is where `/health` and `/ready` live.
#
# The probe is the binary itself, because this image has no `curl` — `packaging/docker` is a
# debian image but the check has to work unchanged in the `scratch` one, and the difference between
# the two should not be whether health checking exists.
#
# The configuration path is an **environment variable rather than an argument**, so a deployment
# that overrides `CMD` to mount its file somewhere else can override this too without deriving a
# new image. `docker run -e VOLUCER_HEALTH_CONFIG=/srv/app/volucer.toml` moves both. A probe given
# `--url` or `--config` explicitly still wins, so debugging by hand is never fought by the default.
ENV VOLUCER_HEALTH_CONFIG=/etc/volucer/volucer.toml

# `/health`, not `/ready`: Docker restarts a container whose health check fails, and a server that
# is merely still booting its worker pool must not be killed for it. `/ready` is the traffic
# question, and answering it here would turn a slow start into a crash loop — the exact inversion
# the two endpoints exist to prevent. Use `volucer health --ready` for the readiness probe an
# orchestrator reads.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/volucer", "health"]

ENTRYPOINT ["/usr/local/bin/volucer"]
CMD ["serve", "--config", "/etc/volucer/volucer.toml"]


# The same binary built against musl instead of glibc: it runs on any Linux, which is what a
# scratch-based image needs and what an operator reaching for `docker run` expects from a static
# build. Two images from one crate rather than one image for each distribution.
FROM rust:1.88-alpine AS build-musl

# The triple is read from the platform rather than spelled out, because this stage is built once
# per architecture: an arm64 builder that was told to produce `x86_64-unknown-linux-musl` would
# either fail at the linker or, worse, cross-compile with a C toolchain that is not there and fail
# somewhere inside `aws-lc-sys`. `TARGETARCH` is set by BuildKit from `--platform`, so a plain
# `docker build` on either machine does the right thing and `--platform` overrides it.
#
# An unknown architecture is a refusal rather than a fallback: a guessed triple produces a
# mislabelled image, which is discovered by whoever pulls it rather than by whoever built it.
#
# `triple` is assigned and read by the same shell, so the `case` and the build are one `RUN`; the
# `cp` to a fixed path is there because `COPY --from` cannot interpolate it further down.
ARG TARGETARCH

RUN apk add --no-cache musl-dev cmake make perl bash nasm

WORKDIR /src
COPY . .

# `TARGETARCH` is BuildKit's, derived from `--platform`. A build that went through the legacy
# builder has it unset, so the builder's own architecture is the fallback rather than a hard
# failure. `uname -m` spells the two the same way Docker does, hence both spellings in the cases.
RUN set -eu; \
    if [ -z "${TARGETARCH:-}" ]; then TARGETARCH="$(uname -m)"; fi; \
    case "${TARGETARCH}" in \
        amd64 | x86_64) triple=x86_64-unknown-linux-musl ;; \
        arm64 | aarch64) triple=aarch64-unknown-linux-musl ;; \
        *) echo "unsupported TARGETARCH ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    rustup target add "${triple}" >/dev/null; \
    cargo build --release --locked -p volucer --target "${triple}"; \
    cp "target/${triple}/release/volucer" /volucer


FROM scratch AS musl

# A static binary still verifies TLS certificates, so it still needs a trust store. There is no
# package manager in a scratch image, so the bundle is copied in and named where the binary looks
# for it (`SSL_CERT_FILE`), which is the one thing a scratch image must be told explicitly.
COPY --from=build-musl /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build-musl /volucer /usr/local/bin/volucer

ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

EXPOSE 8080 9080

# The same probe as the glibc image, and the reason it is the binary rather than `curl`: a `scratch`
# image has no shell and no HTTP client, so `HEALTHCHECK CMD curl -f ...` would be a line that never
# executes. Docker does not report that as a broken check — it reports the container as `unhealthy`
# after the start period with no output, which is the least actionable failure a container can have.
ENV VOLUCER_HEALTH_CONFIG=/etc/volucer/volucer.toml

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/volucer", "health"]

ENTRYPOINT ["/usr/local/bin/volucer"]
CMD ["serve", "--config", "/etc/volucer/volucer.toml"]
