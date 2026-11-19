#!/bin/sh
# Build and lint the workspace the way CI does, on the platform CI uses.
#
# **Why this exists.** Two errors sat on `main` for eight days with every local check green:
# an unread parameter that only disappears under `#[cfg(windows)]`, and a struct literal in a
# file whose first line is `#![cfg(unix)]`. Both compile to nothing on Windows, so `cargo
# check` on a developer's machine passed while `cargo clippy --workspace --all-targets` on
# Ubuntu failed. The gap is not effort — it is that the only machine the CI target *sees* is
# the runner, and nobody looks at it until the push is already made.
#
# So this runs the same commands CI runs, inside a container of the same toolchain. It is the
# local form of the `clippy`, `msrv` and `amqp` jobs, not a replacement for them: CI still
# runs them on a clean machine, which is what proves the result does not depend on this one.
#
# Usage:
#   scripts/check-linux.sh              # clippy + check, the two jobs that were red
#   scripts/check-linux.sh --fmt        # also `cargo fmt --all --check`
#   scripts/check-linux.sh --features   # also the http3 and amqp passes CI makes
#
# Requirements: Docker. The first run pulls the toolchain image and compiles the workspace
# from scratch (a few minutes). Artifacts go to a named volume rather than to the checkout's
# `target/`, so the Linux objects never mix with a host build — see the comment on
# `CARGO_TARGET_DIR` below for why that is not merely tidiness.
#
# **The image tag is deliberately not derived from `rust-toolchain.toml`.** That file pins a
# *channel*; an image tag is a *specific release*, and `rust:1.99-bookworm` resolved to 1.99.1
# when this was written. Naming it explicitly makes the container state which compiler it is
# instead of inheriting whatever the tag floats to — and when the pin moves, this line moves
# with it, which is the moment to re-read both.

set -eu

IMAGE="rust:1.99-bookworm"
TARGET_VOLUME="volucer-linux-target"
REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

want_fmt=false
want_features=false
for arg in "$@"; do
  case "$arg" in
    --fmt) want_fmt=true ;;
    --features) want_features=true ;;
    *)
      echo "unknown argument: $arg" >&2
      echo "usage: $0 [--fmt] [--features]" >&2
      exit 2
      ;;
  esac
done

if ! command -v docker >/dev/null 2>&1; then
  echo "check-linux: docker is not on PATH, so the Linux build cannot be checked here." >&2
  echo "The CI jobs that run it are 'clippy', 'msrv' and 'amqp'." >&2
  exit 1
fi

# The steps CI needs before it can compile, in the order its jobs install them. `aws-lc-sys`
# and `zstd-sys` build C and assembly, so NASM and CMake are build prerequisites rather than
# conveniences; without them the failure surfaces later as a CMake error from inside a
# dependency, which reads like a problem with this repository.
#
# `--all-targets` is load-bearing rather than tidy: a library-only check would have missed the
# `attach.rs` literal entirely, because that file is a *test*. And the second `clippy` pass is
# where the `amqp` job's failure came from, so `--features` is how to reach it.
STEPS=""
if [ "$want_fmt" = true ]; then
  STEPS="$STEPS
cargo fmt --all --check"
fi
STEPS="$STEPS
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets"
if [ "$want_features" = true ]; then
  STEPS="$STEPS
cargo clippy --workspace --all-targets --features http3 -- -D warnings
cargo clippy --workspace --all-targets --features amqp -- -D warnings"
fi

docker run --rm \
  -v "$REPO_ROOT:/w" \
  -v "$TARGET_VOLUME:/linux-target" \
  -w /w \
  -e CARGO_TERM_COLOR=always \
  "$IMAGE" \
  bash -c "set -e
export PATH=/usr/local/cargo/bin:\$PATH
# Artifacts in the container's own volume, never in the bind-mounted \`target/\`. Sharing the
# host directory between two platforms is not just wasted space: a Windows build leaves
# objects, an incremental cache and file locks that a Linux compiler cannot read, and the
# failure arrives as an I/O error on the query cache rather than as a compile error — which
# looks like a broken script instead of a wrong target directory.
export CARGO_TARGET_DIR=/linux-target
apt-get update -qq >/dev/null
apt-get install -y -qq nasm cmake gcc g++ >/dev/null
echo \"--- \$(rustc --version) ---\"
$STEPS
echo '--- all steps passed ---'"
