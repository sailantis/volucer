#!/usr/bin/env bash
#
# Run the installers against a fixture release, on every platform they will ever run on.
#
# The installers are the first thing an operator runs and the last thing anyone tests. A
# `curl | sh` that has never executed is a script that will fail on a stranger's machine,
# in a shell nobody here has, with the failure reported as "installer broken". This runs
# both of them for real: a real archive, a real checksum, a real extraction, and the
# negative cases - because an installer that accepts a corrupted archive is worse than no
# installer at all.
#
# Usage: packaging/install-selftest.sh [path-to-volucer-binary]
#
# Nothing here needs the network, a tag or a release. The fixture is built by
# `stage-archive.sh`, which is the same script the release workflow runs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "${here}/.." && pwd)"
cd "$repo"

case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) os=windows ;;
    Darwin*) os=macos ;;
    *) os=linux ;;
esac

case "$os" in
    windows)
        target=${SELFTEST_TARGET:-x86_64-pc-windows-msvc}
        archive=zip
        exe=.exe
        ;;
    macos)
        target=${SELFTEST_TARGET:-aarch64-apple-darwin}
        archive=tar.gz
        exe=
        ;;
    *)
        target=${SELFTEST_TARGET:-x86_64-unknown-linux-gnu}
        archive=tar.gz
        exe=
        ;;
esac

default_binary="target/release/volucer${exe}"
binary="${1:-$default_binary}"
if [ ! -f "$binary" ]; then
    echo "no binary at ${binary}; run 'cargo build --release -p volucer' first, or pass a path" >&2
    exit 1
fi

version=0.1.0
tag="v${version}"
asset="volucer-${version}-${target}.${archive}"

work="$(mktemp -d)"

# The fixture is a directory laid out exactly like a GitHub release: one tag, the asset, and
# the checksum beside it. Both installers take a base URL, so this is reached by the same
# code path that reaches github.com - not by a stub of it.
release_dir="${work}/releases/download/${tag}"
mkdir -p "$release_dir"

echo "== staging ${asset}"
TARGET="$target" \
    ARCHIVE="$archive" \
    EXE_SUFFIX="$exe" \
    GITHUB_REF_NAME="$tag" \
    BIN="$binary" \
    OUT_DIR="$release_dir" \
    bash packaging/stage-archive.sh >/dev/null

if [ ! -f "${release_dir}/${asset}" ]; then
    echo "stage-archive.sh did not produce ${asset}" >&2
    exit 1
fi

# `latest` resolving to the tag is what the installers' no-version path needs when it is not
# pointed at the API. Here the version is always given explicitly, so the API is never
# reached and the test does not depend on the network.
# The negative fixtures are built here rather than inside the Unix branch, because both
# installers have to refuse them and a guarantee tested on one half is a guarantee for one
# half. `-corrupt` has the right checksum file and a damaged archive; `-nochecksum` has a
# perfect archive and no checksum at all, which is a different failure and must also refuse:
# "verify if a checksum happens to be there" is not verification.
#
# The asset name carries the version, so a fixture published under `v0.1.0-corrupt` has to be
# named `volucer-0.1.0-corrupt-<target>.<archive>` - deriving the name from the tag is what
# both installers do, and a fixture whose name disagrees with its tag would fail at the
# download rather than at the check under test.
fixture() {
    _tag=$1
    _mode=$2
    _dir="${work}/releases/download/${_tag}"
    _name="volucer-${_tag#v}-${target}.${archive}"
    mkdir -p "$_dir"
    cp "${release_dir}/${asset}" "${_dir}/${_name}"
    if [ "$_mode" != nochecksum ]; then
        # The recorded filename inside the checksum is the release's, not this copy's. That
        # is deliberate: both installers read only the digest field, and this is the case
        # that proves it.
        cp "${release_dir}/${asset}.sha256" "${_dir}/${_name}.sha256"
    fi
    if [ "$_mode" = corrupt ]; then
        printf 'x' >>"${_dir}/${_name}"
    fi
}

fixture "${tag}-corrupt" corrupt
fixture "${tag}-nochecksum" nochecksum

# Served over HTTP rather than read from disk. `install.sh` passes `--proto '=https'` to curl
# and `install.ps1` uses `Invoke-WebRequest`; neither accepts `file://`, and relaxing that to
# make the test easier would remove the one guarantee that a plain-text redirect cannot
# downgrade the download. A local server is not a stub: every line of both installers runs,
# and only the origin of the bytes is local. The same choice `volucer-cli` makes with
# `php -S`.
pick_port() {
    python3 -c 'import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()' 2>/dev/null ||
        python -c 'import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()'
}

start_fixture_server() {
    port="$(pick_port)"
    python3 -m http.server "$port" --bind 127.0.0.1 --directory "$work" >"${work}/server.log" 2>&1 &
    server_pid=$!
    # Readiness, not a sleep: the port is known and a refused connection is the signal.
    i=0
    while [ "$i" -lt 100 ]; do
        if python3 -c "import socket, sys
s = socket.socket()
s.settimeout(0.5)
sys.exit(0 if s.connect_ex(('127.0.0.1', ${port})) == 0 else 1)" 2>/dev/null; then
            return 0
        fi
        i=$((i + 1))
        sleep 0.1
    done
    echo "the fixture server never came up; see ${work}/server.log" >&2
    cat "${work}/server.log" >&2 || true
    exit 1
}

start_fixture_server
trap 'rm -rf "$work"; kill "$server_pid" 2>/dev/null || true' EXIT INT TERM
base_url="http://127.0.0.1:${port}/releases/download"

pass=0
fail=0
check() {
    if [ "$1" = ok ]; then
        pass=$((pass + 1))
        echo "   ok: $2"
    else
        fail=$((fail + 1))
        echo "   FAIL: $2" >&2
    fi
}

# --------------------------------------------------------------- install.sh

# `VOLUCER_SH` runs the installer under a different shell. `sh` is `dash` on Debian and Ubuntu
# and `bash` on macOS, so the installer is POSIX `sh` and this asserts that rather than assuming
# it: a script that only ever ran under bash would fail at `set -o pipefail` on the distribution
# most Linux servers actually run.
VOLUCER_SH=${VOLUCER_SH:-sh}

expect_install_sh_ok() {
    destination="${work}/bin-$1"
    if VOLUCER_VERSION="$tag" \
        VOLUCER_INSTALL_DIR="$destination" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out.sh" 2>&1; then
        if [ -f "${destination}/volucer" ] && [ -x "${destination}/volucer" ]; then
            check ok "$1: install.sh installed an executable"
        else
            check fail "$1: install.sh succeeded but installed nothing runnable"
            sed 's/^/      /' "${work}/out.sh" >&2
        fi
    else
        check fail "$1: install.sh failed"
        sed 's/^/      /' "${work}/out.sh" >&2
    fi
}

if [ "$os" != windows ]; then
    echo "== install.sh"

    # The label names the archive this host installs, which is not always `linux-gnu`.
    native_label="linux-gnu"
    if [ "$os" = macos ]; then
        native_label="macos"
    fi
    expect_install_sh_ok "$native_label"

    # The archive the operator gets has to be the one this host would actually run. Running
    # it is the only assertion that proves the platform table and the archive contents agree.
    installed="${work}/bin-${native_label}/volucer"
    if "$installed" version >/dev/null 2>&1; then
        check ok "${native_label}: the installed binary runs"
    else
        check fail "${native_label}: the installed binary does not run"
    fi

    # Only Linux has two libcs, so these assertions are gated rather than rewritten: on macOS
    # `VOLUCER_LIBC=musl` must be *refused*, and asserting the Linux answer there is asserting
    # the wrong thing about the right script. The refusal itself is checked below.
    if [ "$os" = linux ]; then
    # musl is the branch that fails silently at the loader rather than at the installer, so
    # it is asserted from the target string and the refusal rather than inferred from an
    # install succeeding. `VOLUCER_LIBC` forces it, which is the only way to reach it from a
    # glibc host - and the fixture has no musl archive, so the correct outcome here is a
    # refusal that *names the musl asset*. Falling back to the glibc archive would be the one
    # "helpful" behaviour guaranteed to produce a binary that cannot start.
    VOLUCER_VERSION="$tag" \
        VOLUCER_LIBC=musl \
        VOLUCER_INSTALL_DIR="${work}/bin-musl" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-musl.sh" 2>&1 || true

    if grep -q 'x86_64-unknown-linux-musl' "${work}/out-musl.sh"; then
        check ok 'VOLUCER_LIBC=musl selects the musl target'
    else
        check fail 'VOLUCER_LIBC=musl did not select the musl target'
        sed 's/^/      /' "${work}/out-musl.sh" >&2
    fi

    if grep -q 'volucer-.*-x86_64-unknown-linux-musl.tar.gz' "${work}/out-musl.sh" \
        && ! [ -f "${work}/bin-musl/volucer" ]; then
        check ok 'a missing musl archive is refused rather than falling back to glibc'
    else
        check fail 'a missing musl archive was not refused, or something was installed'
        sed 's/^/      /' "${work}/out-musl.sh" >&2
    fi

    # arm64 has no musl build, and the refusal names what does exist rather than constructing
    # an asset name that will 404.
    if VOLUCER_VERSION="$tag" VOLUCER_LIBC=musl VOLUCER_ARCH=aarch64 \
        VOLUCER_INSTALL_DIR="${work}/bin-arm-musl" VOLUCER_DOWNLOAD_URL="$base_url" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-arm-musl.sh" 2>&1; then
        check fail 'arm64 musl was not refused'
    elif grep -q 'no musl archive for Linux on arm64' "${work}/out-arm-musl.sh"; then
        check ok 'arm64 musl is refused by name'
    else
        check fail 'arm64 musl failed for an unexpected reason'
        sed 's/^/      /' "${work}/out-arm-musl.sh" >&2
    fi
    else
        # macOS: there is no second libc, so asking for one must be refused by name rather
        # than silently ignored - a silently ignored override is worse than an unsupported one.
        if VOLUCER_VERSION="$tag" VOLUCER_LIBC=musl VOLUCER_INSTALL_DIR="${work}/bin-musl" \
            VOLUCER_DOWNLOAD_URL="$base_url" \
            "$VOLUCER_SH" packaging/install.sh >"${work}/out-musl.sh" 2>&1; then
            check fail 'VOLUCER_LIBC=musl was accepted on a platform with no musl libc'
        elif grep -q 'makes no sense on' "${work}/out-musl.sh"; then
            check ok 'VOLUCER_LIBC is refused on a platform with only one libc'
        else
            check fail 'the VOLUCER_LIBC refusal failed for the wrong reason'
            sed 's/^/      /' "${work}/out-musl.sh" >&2
        fi
    fi

    echo "== install.sh refuses a corrupted archive"
    if VOLUCER_VERSION="${tag}-corrupt" \
        VOLUCER_INSTALL_DIR="${work}/bin-corrupt" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-corrupt.sh" 2>&1; then
        check fail 'install.sh accepted a corrupted archive'
    elif grep -q 'checksum does not match' "${work}/out-corrupt.sh"; then
        check ok 'a corrupted archive is refused by the checksum'
    else
        check fail 'a corrupted archive failed for the wrong reason'
        sed 's/^/      /' "${work}/out-corrupt.sh" >&2
    fi

    echo "== install.sh refuses a missing checksum"
    if VOLUCER_VERSION="${tag}-nochecksum" \
        VOLUCER_INSTALL_DIR="${work}/bin-nochecksum" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-nochecksum.sh" 2>&1; then
        check fail 'install.sh installed without a checksum'
    else
        check ok 'a missing checksum is refused'
    fi

    # An existing binary is kept unless --force is asked for, which is the half of "leaves an
    # existing binary exactly as it was" that is testable without an interrupted download.
    echo "== install.sh refuses to overwrite"
    if VOLUCER_VERSION="$tag" \
        VOLUCER_INSTALL_DIR="${work}/bin-${native_label}" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-overwrite.sh" 2>&1; then
        check fail 'install.sh overwrote an existing binary without being asked'
    elif grep -q 'already exists' "${work}/out-overwrite.sh"; then
        check ok 'an existing binary is not replaced without VOLUCER_FORCE'
    else
        check fail 'the overwrite refusal failed for the wrong reason'
        sed 's/^/      /' "${work}/out-overwrite.sh" >&2
    fi
else
    echo "== install.sh is not exercised on Windows (it is the Unix half)"
fi

# --------------------------------------------------------------- install.ps1

if command -v pwsh >/dev/null 2>&1 || command -v powershell >/dev/null 2>&1; then
    echo "== install.ps1"
    ps="$(command -v pwsh || command -v powershell)"

    # PowerShell is a native Windows program even when it is launched from Git Bash, so it
    # resolves neither `/c/...` nor a bare `/`-rooted path. `cygpath -w` is present wherever
    # Git Bash is, and the fallback is the identity so this stays runnable under WSL, where
    # the paths PowerShell is given are never used because `powershell` is not on PATH there.
    to_windows() {
        if command -v cygpath >/dev/null 2>&1; then
            cygpath -w "$1"
        else
            printf '%s' "$1"
        fi
    }
    script_win="$(to_windows "${here}/install.ps1")"
    work_win="$(to_windows "${work}")"
    # `-DestinationPath` and `Get-FileHash` are 5.1-compatible, so the version chosen here is
    # whichever is present rather than whichever supports a newer spelling.
    if [ "$os" = windows ]; then
        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            \$destination = '${work_win}/ps-bin'
            & '${script_win}' -Version '${tag}' -InstallDir \$destination -BaseUrl '${base_url}'
            if (-not (Test-Path (Join-Path \$destination 'volucer.exe'))) {
                Write-Error 'install.ps1 succeeded but installed nothing'
                exit 1
            }
        " >"${work}/out-ps.txt" 2>&1; then
            check ok 'install.ps1 installed an executable'
        else
            check fail 'install.ps1 failed'
            sed 's/^/      /' "${work}/out-ps.txt" >&2
        fi

        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            & '${script_win}' -Version '${tag}-corrupt' -InstallDir '${work_win}/ps-corrupt' -BaseUrl '${base_url}'
        " >"${work}/out-ps-corrupt.txt" 2>&1; then
            check fail 'install.ps1 accepted a corrupted archive'
        elif grep -qi 'checksum does not match' "${work}/out-ps-corrupt.txt"; then
            check ok 'install.ps1 refuses a corrupted archive'
        else
            check fail 'install.ps1 failed on a corrupted archive for the wrong reason'
            sed 's/^/      /' "${work}/out-ps-corrupt.txt" >&2
        fi

        # The documented one-liner is `irm ... | iex`, which cannot pass a single parameter, so
        # environment variables are the *only* way to redirect it -- and every case above passes
        # `-InstallDir` explicitly, which is precisely why none of them noticed that
        # `VOLUCER_INSTALL_DIR` was ignored. The shell installer reads these names, so the
        # PowerShell one has to agree about them or the two disagree silently.
        #
        # `VOLUCER_VERSION` is part of the same check on purpose, and it is the one that found a
        # second defect: the installer resolved the version from the releases API *before*
        # reading the variable, so a pinned version was still resolved over the network. Passing
        # all three means this case never touches the network and a pinned version is actually
        # pinned -- and it is why this check no longer depends on GitHub's rate limit. It failed
        # in CI with a 403 from api.github.com and was read as "the variable is ignored".
        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            \$env:VOLUCER_VERSION = '${tag}'
            \$env:VOLUCER_INSTALL_DIR = '${work_win}/ps-env'
            \$env:VOLUCER_DOWNLOAD_URL = '${base_url}'
            & '${script_win}'
            if (-not (Test-Path (Join-Path '${work_win}/ps-env' 'volucer.exe'))) {
                Write-Error 'VOLUCER_INSTALL_DIR was ignored'
                exit 1
            }
        " >"${work}/out-ps-env.txt" 2>&1; then
            check ok 'install.ps1 honours VOLUCER_INSTALL_DIR, which is all a one-liner can set'
        else
            check fail 'install.ps1 ignored the environment variables a one-liner has to use'
            sed 's/^/      /' "${work}/out-ps-env.txt" >&2
        fi

        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-overwrite' -BaseUrl '${base_url}'
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-overwrite' -BaseUrl '${base_url}'
        " >"${work}/out-ps-overwrite.txt" 2>&1; then
            check fail 'install.ps1 overwrote an existing binary without -Force'
        elif grep -qi 'already exists' "${work}/out-ps-overwrite.txt"; then
            check ok 'an existing binary is not replaced without -Force'
        else
            check fail 'the PowerShell overwrite refusal failed for the wrong reason'
            sed 's/^/      /' "${work}/out-ps-overwrite.txt" >&2
        fi

        # A pinned version must be resolved *without* the network, and must be the version
        # actually used. Both halves matter: this check pins a tag that is deliberately *not*
        # the latest one, because pinning the latest cannot tell "the variable was honoured"
        # from "the API was asked and happened to agree". Before the ordering was fixed this
        # printed `release v0.1.0` for a pin of `v0.0.9`.
        "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Continue'
            \$env:VOLUCER_VERSION = 'v0.0.9'
            \$env:VOLUCER_DOWNLOAD_URL = '${base_url}'
            & '${script_win}' -InstallDir '${work_win}/ps-pinned'
        " >"${work}/out-ps-pinned.txt" 2>&1 || true
        # The exit status is deliberately not part of this: the download is expected to 404,
        # because the fixture serves one tag and this pins a different one, and PowerShell runs
        # an `exit` trap on the way out so the status says nothing about the resolution. The
        # assertion is the line printed *before* any request — `release v0.0.9` — which is the
        # only place the ordering is visible.
        if grep -q 'release v0.0.9' "${work}/out-ps-pinned.txt"; then
            if grep -qi 'releases API' "${work}/out-ps-pinned.txt"; then
                check fail 'install.ps1 asked the releases API for a version it was already given'
                sed 's/^/      /' "${work}/out-ps-pinned.txt" >&2
            else
                check ok 'install.ps1 uses a pinned VOLUCER_VERSION without consulting the API'
            fi
        else
            check fail 'install.ps1 did not use the pinned version'
            sed 's/^/      /' "${work}/out-ps-pinned.txt" >&2
        fi
    else
        # The positive path is Windows-only, and this is the assertion that keeps the script
        # honest on the platforms it is not for: it must refuse by name rather than select an
        # x86_64-pc-windows-msvc archive that downloads, verifies, extracts and cannot run.
        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-wrong-os' -BaseUrl '${base_url}'
        " >"${work}/out-ps-wrong-os.txt" 2>&1; then
            check fail 'install.ps1 installed on a non-Windows host'
        elif grep -qi 'no archive for Windows on' "${work}/out-ps-wrong-os.txt"; then
            check ok 'install.ps1 refuses a non-Windows host by name'
        else
            check fail 'install.ps1 failed on a non-Windows host for an unexpected reason'
            sed 's/^/      /' "${work}/out-ps-wrong-os.txt" >&2
        fi
    fi
else
    echo "== install.ps1 not exercised: no PowerShell found"
fi

echo
if [ "$fail" -ne 0 ]; then
    echo "${fail} install check(s) failed, ${pass} passed" >&2
    exit 1
fi
echo "ok: ${pass} install checks passed"
