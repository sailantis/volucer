#!/usr/bin/env bash
#
# Prove that an example image is what it claims to be, by running it.
#
# Usage: examples/docker/verify.sh <image> [<volucer-toml>]
#
# The recipe in a documentation page is a claim. This script is what turns it into a fact: `ci.yml`
# runs it against every image the examples build, so a line that stops working — a package that is
# renamed, a PHP version that moves, a binary that is no longer where the `COPY` says it is —
# fails a pull request instead of a user's first `docker run`.
#
# It checks four things, each of which has a way of being silently wrong:
#
#   1. **The image is what the documentation says it is.** `php` is on `PATH` and runs, `volucer`
#      runs, and `Volucer\Supervisor` can fork — the last one is `pcntl`, which Alpine and Ubuntu
#      package in opposite ways.
#   2. **`volucer check` resolves the backend.** It looks up the `command` of a `worker` backend
#      on `PATH`. An image that installs `/usr/bin/php85` and no `php` symlink passes every
#      `docker run --entrypoint php` above and still starts a backend that cannot exec.
#   3. **The health check answers both ways.** A probe that only ever succeeds is a probe that
#      passes on a container whose server died.
#   4. **A request is served.** The image's own `ENTRYPOINT` starts a server that answers over
#      HTTP, which is the one thing none of the assertions above can tell you.
set -euo pipefail

image="${1:?usage: verify.sh <image> [<volucer-toml>]}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
config="${2:-$here/volucer.toml}"
name="volucer-example-$$"

cleanup() {
    docker rm -f "$name" >/dev/null 2>&1 || true
    rm -rf "${work:-}"
}
trap cleanup EXIT

step() { printf 'verify: %s\n' "$*" >&2; }
fail() { printf 'verify: FAILED: %s\n' "$*" >&2; exit 1; }

# Git Bash, MSYS and Cygwin rewrite an argument that starts with `/` into a Windows path, so
# `-v "$work/volucer.toml:/etc/volucer/volucer.toml:ro"` becomes a mount of
# `C:/Program Files/Git/etc/...` and every in-container path below is mangled the same way.
# `MSYS_NO_PATHCONV` is read by those shells and ignored by Linux and macOS.
export MSYS_NO_PATHCONV=1

work="$(mktemp -d)"

# Docker Desktop on Windows and Git Bash disagree with `mktemp -d` about what a path is; `cygpath
# -m` gives Docker a form it can bind-mount. Linux and macOS have no `cygpath`, so the path is
# used unchanged there.
hostpath() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi
}

step "${image}: php is on PATH and runs"
docker run --rm --entrypoint php "$image" -r 'echo PHP_VERSION, PHP_EOL;' \
    || fail "no 'php' on PATH; a worker backend with command = \"php worker.php\" cannot start"

step "${image}: pcntl is available (the attach backend forks)"
docker run --rm --entrypoint php "$image" -r 'exit(function_exists("pcntl_fork") ? 0 : 1);' \
    || fail "pcntl_fork() is missing; Supervisor::serve() would refuse to run"

step "${image}: a unix socket can be opened (the attach relay)"
docker run --rm --entrypoint php "$image" \
    -r 'exit(@stream_socket_server("unix:///tmp/verify.sock") === false ? 1 : 0);' \
    || fail "stream_socket_server() cannot open a unix socket"

step "${image}: volucer runs"
docker run --rm --entrypoint volucer "$image" version >/dev/null \
    || fail "volucer does not run"

step "${image}: volucer check resolves the backend's php"
# A copy of the configuration, because `check` reports what it found on *this* host and the
# document root of the fixture is the image's own `/srv/app/public`.
cp "$config" "$work/volucer.toml"
chmod 0644 "$work/volucer.toml"
docker run --rm --entrypoint volucer -v "$(hostpath "$work")/volucer.toml:/etc/volucer/volucer.toml:ro" \
    "$image" check --config /etc/volucer/volucer.toml >"$work/check.out" 2>&1 \
    || { cat "$work/check.out" >&2; fail "volucer check rejected the configuration"; }
if grep -qiE 'the worker program .* does not exist|from command .* does not exist' "$work/check.out"; then
    cat "$work/check.out" >&2
    fail "volucer check could not find the program a backend's command names"
fi
grep -qi 'cli  PHP' "$work/check.out" || fail "volucer check did not find a PHP CLI binary"

step "${image}: starting the server and waiting for its health check"
docker run -d --name "$name" \
    -p 127.0.0.1::8080 -p 127.0.0.1::9080 \
    -v "$(hostpath "$work")/volucer.toml:/etc/volucer/volucer.toml:ro" \
    --health-interval=2s --health-timeout=2s --health-retries=10 --health-start-period=5s \
    "$image" >/dev/null

status=starting
for _ in $(seq 1 90); do
    status="$(docker inspect --format '{{.State.Health.Status}}' "$name" 2>/dev/null || echo gone)"
    [ "$status" = healthy ] && break
    [ "$status" = gone ] && break
    sleep 1
done

if [ "$status" != healthy ]; then
    echo "verify: the image's HEALTHCHECK reported '${status}'" >&2
    docker inspect --format '{{json .State.Health}}' "$name" >&2 || true
    docker logs "$name" >&2 || true
    fail "the container did not become healthy"
fi

step "${image}: the health check must fail against a closed port"
# Port 1 is not listening, which is the shape of a container whose server has died. Without this,
# the check above also passes on a probe that returns success unconditionally.
if docker exec -e VOLUCER_HEALTH_URL=http://127.0.0.1:1/health "$name" \
        /usr/local/bin/volucer health >/dev/null 2>&1; then
    fail "the health check passed against a closed port"
fi

step "${image}: a request is served"
# The port Docker published, so two runs — glibc and musl, or two CI jobs — do not race for 8080.
port="$(docker port "$name" 8080/tcp | head -1 | sed 's/.*://')"
[ -n "$port" ] || fail "no published port for 8080; the container may have exited"

if command -v curl >/dev/null 2>&1; then
    body="$(curl -fsS "http://127.0.0.1:${port}/verify" 2>&1 || true)"
else
    body="$(docker run --rm --network "container:${name}" --entrypoint php "$image" \
        -r "echo @file_get_contents('http://127.0.0.1:8080/verify');" 2>&1 || true)"
fi
case "$body" in
    *"hello from"*) ;;
    *)
        docker logs "$name" >&2 || true
        fail "the server did not answer a request on port ${port}: ${body:-<empty>}"
        ;;
esac

step "${image}: ok"
