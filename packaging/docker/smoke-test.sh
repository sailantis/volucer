#!/usr/bin/env bash
#
# Prove that an image is what it claims to be, by running it.
#
# Usage: packaging/docker/smoke-test.sh <image>
#
# `<image>` is anything `docker run` accepts: a tag (`volucer:musl`) or, in the release workflow,
# a digest (`ghcr.io/sailantis/volucer@sha256:...`) so that what is tested is byte-for-byte what
# will be published rather than a local rebuild of it.
#
# Two things are checked, and both have failed in real images:
#
#   1. The binary runs at all. A `scratch` image can be built with a missing trust store and still
#      `docker run`, and then fail every outbound TLS connection at run time — a missing
#      `ca-certificates.crt` does not stop `version` from printing.
#   2. The `HEALTHCHECK` answers *both* ways. A probe that only ever succeeds is a probe that
#      passes on a container whose server died, and a `scratch` image is exactly where a
#      `HEALTHCHECK CMD curl` would be a line that never executes and never reports why.
#
# The health check is exercised through Docker's own scheduler rather than by invoking the
# subcommand directly: the thing under test is the `HEALTHCHECK` line in the Dockerfile, which
# includes its interval, timeout and start period.
set -euo pipefail

image="${1:?usage: smoke-test.sh <image>}"
name="volucer-smoke-$$"
work="$(mktemp -d)"

cleanup() {
    docker rm -f "$name" >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT

step() { printf 'smoke-test: %s\n' "$*" >&2; }

# A path Docker can bind-mount. On Git Bash, MSYS and Cygwin, `mktemp -d` answers
# `/tmp/tmp.XXXX`, which is a path inside the *emulated* filesystem and not one Docker Desktop
# can see: the mount silently resolves to an empty directory and the container reports a missing
# configuration file, which reads as a broken image rather than as a wrong path. `cygpath -m`
# converts to the Windows form with forward slashes, which both Docker and `-v` accept.
# Linux and macOS have no `cygpath`, so the path is used unchanged there.
hostpath() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -m "$1"
    else
        printf '%s' "$1"
    fi
}

step "${image}: running the binary"
docker run --rm "$image" version

# A configuration file whose two listeners are bound to `0.0.0.0`, so that the probe's own address
# is reachable from inside the container. `[status]` is what `volucer health` reads when it is
# given no `--url`; a `[server]` table is required by `serve`, and `document_root = "/"` keeps the
# fixture to the one file.
#
# The directory and the file are opened up on purpose: `mktemp -d` creates a `0700` directory owned
# by the invoking user, and on a **Linux** Docker host the glibc image's unprivileged `volucer` user
# cannot traverse it - the bind mount resolves to a permission error and the server reports a
# missing configuration file rather than anything about the image. Docker Desktop's WSL2 mounts
# ignore the mode bits, so this only shows up on Linux CI or a Linux workstation, which is exactly
# where a check like this has to work.
mkdir -p "$work"
chmod 0755 "$work"
cat >"$work/volucer.toml" <<'EOF'
[server]
http = "0.0.0.0:8080"
document_root = "/"

[status]
address = "0.0.0.0:9080"
EOF
chmod 0644 "$work/volucer.toml"

step "${image}: starting a server and waiting for its health check"
docker run -d --name "$name" \
    -v "$(hostpath "$work")/volucer.toml:/etc/volucer/volucer.toml:ro" \
    --health-interval=2s --health-timeout=2s --health-retries=10 \
    --health-start-period=5s \
    "$image" >/dev/null

# A poll rather than a sleep: the start period is a bound, not a duration, and a slow machine
# should take longer rather than fail.
status=starting
for _ in $(seq 1 90); do
    status="$(docker inspect --format '{{.State.Health.Status}}' "$name")"
    [ "$status" = healthy ] && break
    sleep 1
done

if [ "$status" != healthy ]; then
    echo "smoke-test: the image's HEALTHCHECK reported '${status}'" >&2
    docker inspect --format '{{json .State.Health}}' "$name" >&2 || true
    docker logs "$name" >&2 || true
    exit 1
fi

# The check has to fail too, or it is a probe that always passes. Port 1 is not listening inside the
# container, which is the shape of a container whose server has died. The absolute path to the
# binary, because a `scratch` image has no `PATH` for `docker exec` to search.
#
# `MSYS_NO_PATHCONV=1` is for Git Bash, which rewrites an argument that starts with `/` into a
# Windows path: `docker exec ... /usr/local/bin/volucer` becomes
# `C:/Program Files/Git/usr/local/bin/volucer` and fails to exec at all. That failure is
# non-zero, so without the variable this step would pass while testing nothing - which is why the
# stderr is also inspected below. MSYS reads the variable; Linux and macOS ignore it.
step "${image}: the health check must fail against a closed port"
if MSYS_NO_PATHCONV=1 docker exec -e VOLUCER_HEALTH_URL=http://127.0.0.1:1/health "$name" \
        /usr/local/bin/volucer health 2>"$work/exec.err"; then
    echo "smoke-test: the health check passed against a closed port" >&2
    exit 1
fi
if grep -qiE 'unable to start container process|OCI runtime exec failed|executable file not found' \
        "$work/exec.err"; then
    echo "smoke-test: the negative check did not run the binary at all:" >&2
    cat "$work/exec.err" >&2
    exit 1
fi

step "${image}: ok"
