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
        HOME="${work}/home-$1" \
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

    # ----------------------------------------------------- install.sh and PATH

    # The installer edits a dotfile by default, so these assertions are also the only thing
    # keeping the suite from editing the machine it runs on: every case below sets HOME to a
    # directory under $work and clears the directory out of PATH, which is the state a fresh
    # account is in. `SHELL` is set explicitly rather than inherited, because the file chosen
    # is a function of it and a CI runner's SHELL is not necessarily the operator's.
    echo "== install.sh adds the install directory to PATH"

    path_install() {
        _dest=$1
        shift
        env -u VOLUCER_MODIFY_PATH \
            VOLUCER_VERSION="$tag" \
            VOLUCER_INSTALL_DIR="$_dest" \
            VOLUCER_DOWNLOAD_URL="$base_url" \
            VOLUCER_FORCE=1 \
            "$@" \
            "$VOLUCER_SH" packaging/install.sh 2>&1
    }

    # Not on PATH: the block is appended, and the output names the file it was appended to -
    # which is the whole difference between a hidden edit and one the operator can undo.
    # `.bashrc` is created first because every distribution that ships bash ships one, and it
    # is the file an interactive non-login shell reads.
    home_path="${work}/home-path"
    mkdir -p "$home_path"
    printf '# ~/.bashrc\n' >"$home_path/.bashrc"
    out=$(path_install "${work}/bin-path" HOME="$home_path" SHELL=/bin/bash \
        PATH="/usr/bin:/bin")
    if grep -q 'export PATH=' "$home_path/.bashrc" 2>/dev/null; then
        check ok 'a missing install directory is added to the shell profile'
    else
        check fail 'the install directory was not added to the shell profile'
        printf '%s\n' "$out" >&2
    fi

    if printf '%s\n' "$out" | grep -q "${home_path}/.bashrc"; then
        check ok 'the installer names the file it edited'
    else
        check fail 'the installer edited a file without naming it'
        printf '%s\n' "$out" >&2
    fi

    if printf '%s\n' "$out" | grep -qi 'undo'; then
        check ok 'the installer says how to undo the edit'
    else
        check fail 'the installer edited a dotfile without saying how to undo it'
        printf '%s\n' "$out" >&2
    fi

    # Re-running is the normal case - the second release someone installs. A second copy of the
    # line is harmless in PATH and wrong in a file, and the test for it is the count.
    before=$(grep -c 'export PATH=' "$home_path/.bashrc")
    out=$(path_install "${work}/bin-path" HOME="$home_path" SHELL=/bin/bash \
        PATH="/usr/bin:/bin")
    after=$(grep -c 'export PATH=' "$home_path/.bashrc")
    if [ "$before" = "$after" ]; then
        check ok 'a second install does not append a duplicate line'
    else
        check fail 'a second install appended the line again'
        printf '%s\n' "$out" >&2
    fi

    # A bash with no `.bashrc` - a stripped-down server image, or macOS before anyone has
    # opened Terminal - falls back to `~/.profile`, which is what a login shell reads. Creating
    # a `.bashrc` there instead would leave the line unread by the login shells that are the
    # norm on macOS.
    home_norc="${work}/home-norc"
    mkdir -p "$home_norc"
    out=$(path_install "${work}/bin-norc" HOME="$home_norc" SHELL=/bin/bash \
        PATH="/usr/bin:/bin")
    if [ ! -e "$home_norc/.bashrc" ] && grep -q 'export PATH=' "$home_norc/.profile" 2>/dev/null; then
        check ok 'bash without a .bashrc falls back to .profile'
    else
        check fail 'bash without a .bashrc did not fall back to .profile'
        printf '%s\n' "$out" >&2
    fi

    # A stock Debian or Ubuntu `~/.profile` already contains this, spelled `$HOME/.local/bin`
    # rather than expanded. The default destination is exactly that directory, so a host that
    # ships the line must not get a second one - and the check cannot be a substring search for
    # the expanded path, because the expanded path is not what is written there.
    home_debian="${work}/home-debian"
    mkdir -p "$home_debian"
    printf '# ~/.profile\nif [ -d "$HOME/.local/bin" ] ; then\n    PATH="$HOME/.local/bin:$PATH"\nfi\n' \
        >"$home_debian/.profile"
    cp "$home_debian/.profile" "${work}/profile-debian.before"
    out=$(path_install "$home_debian/.local/bin" HOME="$home_debian" SHELL=/bin/sh \
        PATH="/usr/bin:/bin")
    if cmp -s "${work}/profile-debian.before" "$home_debian/.profile"; then
        check ok 'a profile that already adds the directory is left alone'
    else
        check fail 'the installer duplicated a PATH line a stock profile already had'
        printf '%s\n' "$out" >&2
        diff "${work}/profile-debian.before" "$home_debian/.profile" >&2 || true
    fi

    # A mention in a comment is not a PATH entry. A file that says "# to add it: export
    # PATH=$HOME/.local/bin:$PATH" tells the operator nothing by being left alone, and "open a
    # new shell" is then the wrong answer - the new shell will not find the binary either.
    home_comment="${work}/home-comment"
    mkdir -p "$home_comment"
    printf '# add it yourself: export PATH="$HOME/.local/bin:$PATH"\n' >"$home_comment/.bashrc"
    out=$(path_install "$home_comment/.local/bin" HOME="$home_comment" SHELL=/bin/bash \
        PATH="/usr/bin:/bin")
    if [ "$(grep -c '^export PATH=' "$home_comment/.bashrc")" -eq 1 ]; then
        check ok 'a commented-out PATH line does not count as already added'
    else
        check fail 'a comment mentioning the directory suppressed the real edit'
        printf '%s\n' "$out" >&2
        sed 's/^/      /' "$home_comment/.bashrc" >&2
    fi

    # Opt-out, which is what CI and a managed host need: the binary is installed, nothing else
    # on the machine changes, and the operator is told what to add themselves.
    home_optout="${work}/home-optout"
    mkdir -p "$home_optout"
    out=$(env VOLUCER_MODIFY_PATH=0 \
        VOLUCER_VERSION="$tag" \
        VOLUCER_INSTALL_DIR="${work}/bin-optout" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        HOME="$home_optout" SHELL=/bin/bash PATH="/usr/bin:/bin" \
        "$VOLUCER_SH" packaging/install.sh 2>&1)
    if [ -f "${work}/bin-optout/volucer" ] && [ ! -e "$home_optout/.bashrc" ]; then
        check ok 'VOLUCER_MODIFY_PATH=0 installs without touching the profile'
    else
        check fail 'VOLUCER_MODIFY_PATH=0 still wrote to the profile, or did not install'
        printf '%s\n' "$out" >&2
    fi

    # A dotfiles repository owns the profile through a symlink. Appending to the link would
    # replace it with a regular file and lose the repository's copy at the next apply, so the
    # target is what must be written and the link must survive as a link.
    home_link="${work}/home-link"
    mkdir -p "$home_link/dotfiles"
    printf '# managed by a dotfiles repository\n' >"$home_link/dotfiles/bashrc"
    ln -s dotfiles/bashrc "$home_link/.bashrc"
    out=$(path_install "${work}/bin-link" HOME="$home_link" SHELL=/bin/bash \
        PATH="/usr/bin:/bin")
    if [ -L "$home_link/.bashrc" ] &&
        grep -q 'export PATH=' "$home_link/dotfiles/bashrc" 2>/dev/null; then
        check ok 'a symlinked profile is written through, not replaced'
    else
        check fail 'a symlinked profile was replaced or not written'
        printf '%s\n' "$out" >&2
    fi

    # fish does not read `.bashrc`, and its syntax for PATH is not `export`. A line in the
    # wrong file is an edit that does nothing, which is worse than no edit at all.
    home_fish="${work}/home-fish"
    mkdir -p "$home_fish"
    out=$(path_install "${work}/bin-fish" HOME="$home_fish" SHELL=/usr/bin/fish \
        PATH="/usr/bin:/bin")
    if grep -q 'set -gx PATH' "$home_fish/.config/fish/config.fish" 2>/dev/null; then
        check ok 'fish gets a line in config.fish, in fish syntax'
    else
        check fail 'fish did not get a usable PATH line'
        printf '%s\n' "$out" >&2
    fi

    # zsh is macOS's default shell and reads `.zshrc` in a login shell, while `.zprofile` is
    # skipped by a non-login terminal - so `.zshrc` is the file, and `~/.profile` is not.
    # `ZDOTDIR` moves it, and a zsh user who has moved it does not read `$HOME/.zshrc` at all.
    home_zsh="${work}/home-zsh"
    mkdir -p "$home_zsh/zdot"
    out=$(path_install "${work}/bin-zsh" HOME="$home_zsh" SHELL=/bin/zsh \
        ZDOTDIR="$home_zsh/zdot" PATH="/usr/bin:/bin")
    if grep -q 'export PATH=' "$home_zsh/zdot/.zshrc" 2>/dev/null &&
        [ ! -e "$home_zsh/.profile" ]; then
        check ok 'zsh gets a line in .zshrc, and ZDOTDIR is honoured'
    else
        check fail 'zsh did not get a line in the file it actually reads'
        printf '%s\n' "$out" >&2
    fi

    # Already on PATH is the case that must do nothing: a container image, a host where the
    # directory was added long ago, or a rerun in the same shell.
    home_onpath="${work}/home-onpath"
    mkdir -p "$home_onpath"
    out=$(path_install "${work}/bin-onpath" HOME="$home_onpath" SHELL=/bin/bash \
        PATH="${work}/bin-onpath:/usr/bin:/bin")
    if [ ! -e "$home_onpath/.bashrc" ]; then
        check ok 'a directory already on PATH is not written to any profile'
    else
        check fail 'a directory already on PATH was added to a profile anyway'
        printf '%s\n' "$out" >&2
    fi

    # A misspelt mode is refused before the download, like every other setting, rather than
    # being read as "off" or "on" by whichever branch happens to be first.
    out=$(env VOLUCER_MODIFY_PATH=yes \
        VOLUCER_VERSION="$tag" \
        VOLUCER_INSTALL_DIR="${work}/bin-modify-bad" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        HOME="${work}/home-modify-bad" SHELL=/bin/bash PATH="/usr/bin:/bin" \
        "$VOLUCER_SH" packaging/install.sh 2>&1) && status=0 || status=$?
    if [ "$status" -ne 0 ]; then
        check ok 'an unknown VOLUCER_MODIFY_PATH is refused'
    else
        check fail 'an unknown VOLUCER_MODIFY_PATH was accepted'
        printf '%s\n' "$out" >&2
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
        HOME="${work}/home-musl" \
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
        HOME="${work}/home-arm-musl" \
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
            HOME="${work}/home-musl-macos" \
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
        HOME="${work}/home-corrupt" \
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
        HOME="${work}/home-nochecksum" \
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
        HOME="${work}/home-overwrite" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-overwrite.sh" 2>&1; then
        check fail 'install.sh overwrote an existing binary without being asked'
    elif grep -q 'already exists' "${work}/out-overwrite.sh"; then
        check ok 'an existing binary is not replaced without VOLUCER_FORCE'
    else
        check fail 'the overwrite refusal failed for the wrong reason'
        sed 's/^/      /' "${work}/out-overwrite.sh" >&2
    fi

    # The provenance gate. The fixture origin is not GitHub, so `auto` must say it did not check
    # rather than pretending it did - a silent skip is the failure mode a verification step is
    # most likely to acquire - and `require` must refuse, because there is nothing to verify
    # against. Neither may fail the install on a mirror, which is the documented override.
    echo "== install.sh reports that provenance is not checked on a mirror"
    if VOLUCER_VERSION="$tag" \
        VOLUCER_INSTALL_DIR="${work}/bin-verify-auto" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        VOLUCER_VERIFY=auto \
        HOME="${work}/home-verify-auto" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-verify-auto.sh" 2>&1; then
        if grep -q 'provenance not checked' "${work}/out-verify-auto.sh"; then
            check ok 'auto says plainly that provenance was not checked'
        else
            check fail 'auto installed from a mirror without saying provenance was unchecked'
            sed 's/^/      /' "${work}/out-verify-auto.sh" >&2
        fi
    else
        check fail 'auto refused to install from a mirror; that is a supported override'
        sed 's/^/      /' "${work}/out-verify-auto.sh" >&2
    fi

    echo "== install.sh requires provenance when it is asked to"
    if VOLUCER_VERSION="$tag" \
        VOLUCER_INSTALL_DIR="${work}/bin-verify-require" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        VOLUCER_VERIFY=require \
        HOME="${work}/home-verify-require" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-verify-require.sh" 2>&1; then
        check fail 'require installed from a mirror, where nothing could be verified'
    elif grep -q 'no attestation to check' "${work}/out-verify-require.sh"; then
        check ok 'require refuses a mirror by name'
    else
        check fail 'the require refusal failed for the wrong reason'
        sed 's/^/      /' "${work}/out-verify-require.sh" >&2
    fi

    echo "== install.sh rejects an unknown verification mode"
    if VOLUCER_VERSION="$tag" \
        VOLUCER_INSTALL_DIR="${work}/bin-verify-bad" \
        VOLUCER_DOWNLOAD_URL="$base_url" \
        VOLUCER_VERIFY=sometimes \
        HOME="${work}/home-verify-bad" \
        "$VOLUCER_SH" packaging/install.sh >"${work}/out-verify-bad.sh" 2>&1; then
        check fail 'an unknown VOLUCER_VERIFY was accepted'
    elif grep -q 'expected "auto"' "${work}/out-verify-bad.sh"; then
        check ok 'an unknown VOLUCER_VERIFY is refused by name'
    else
        check fail 'the unknown-mode refusal failed for the wrong reason'
        sed 's/^/      /' "${work}/out-verify-bad.sh" >&2
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
        # `-ModifyPath 0` on every case: these install into a temporary directory that the
        # `finally` block deletes, so adding it to the machine's user PATH leaves an entry
        # that names nothing - permanently, once per run. That is not hypothetical: it had
        # accumulated 43 of them before anyone counted.
        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            \$destination = '${work_win}/ps-bin'
            & '${script_win}' -Version '${tag}' -InstallDir \$destination -BaseUrl '${base_url}' -ModifyPath 0
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

        # The binary is installed and the machine's environment is untouched, which is what an
        # unattended install has to promise. Asserting it here is the only thing that keeps the
        # suite from writing to the PATH of whichever machine runs it.
        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            \$before = [Environment]::GetEnvironmentVariable('Path', 'User')
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-modify-off' -BaseUrl '${base_url}' -ModifyPath 0
            \$after = [Environment]::GetEnvironmentVariable('Path', 'User')
            if (\$after -ne \$before) {
                Write-Error 'the user PATH was edited despite -ModifyPath 0'
                exit 1
            }
        " >"${work}/out-ps-modify-off.txt" 2>&1; then
            check ok 'install.ps1 leaves the user PATH alone with -ModifyPath 0'
        else
            check fail 'install.ps1 edited the user PATH despite -ModifyPath 0'
            sed 's/^/      /' "${work}/out-ps-modify-off.txt" >&2
        fi

        # And the default still does what the README says. It is run against the *user* PATH
        # because that is the value the installer writes; the entry is removed again so the
        # suite leaves the machine as it found it.
        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            \$dir = '${work_win}/ps-modify-on'
            \$before = [Environment]::GetEnvironmentVariable('Path', 'User')
            try {
                & '${script_win}' -Version '${tag}' -InstallDir \$dir -BaseUrl '${base_url}'
                \$after = [Environment]::GetEnvironmentVariable('Path', 'User')
                if (\$after -notlike \"*\$dir*\") {
                    Write-Error 'the install directory was not added to the user PATH'
                    exit 1
                }
            } finally {
                [Environment]::SetEnvironmentVariable('Path', \$before, 'User')
            }
        " >"${work}/out-ps-modify-on.txt" 2>&1; then
            check ok 'install.ps1 adds the install directory to the user PATH by default'
        else
            check fail 'install.ps1 did not add the install directory to the user PATH'
            sed 's/^/      /' "${work}/out-ps-modify-on.txt" >&2
        fi

        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            & '${script_win}' -Version '${tag}-corrupt' -InstallDir '${work_win}/ps-corrupt' -BaseUrl '${base_url}' -ModifyPath 0
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
            \$env:VOLUCER_MODIFY_PATH = '0'
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
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-overwrite' -BaseUrl '${base_url}' -ModifyPath 0
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-overwrite' -BaseUrl '${base_url}' -ModifyPath 0
        " >"${work}/out-ps-overwrite.txt" 2>&1; then
            check fail 'install.ps1 overwrote an existing binary without -Force'
        elif grep -qi 'already exists' "${work}/out-ps-overwrite.txt"; then
            check ok 'an existing binary is not replaced without -Force'
        else
            check fail 'the PowerShell overwrite refusal failed for the wrong reason'
            sed 's/^/      /' "${work}/out-ps-overwrite.txt" >&2
        fi

        # The provenance gate, asserted here for the same reason the corrupted archive is: a
        # verification step that silently does nothing is indistinguishable from one that passed.
        # The fixture origin is not GitHub, so `auto` must say so and `require` must refuse — and
        # both before the download, which is why the negative case asserts the absence of
        # 'downloading' as well as the message.
        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-verify-auto' -BaseUrl '${base_url}' -Verify auto -ModifyPath 0
        " >"${work}/out-ps-verify-auto.txt" 2>&1; then
            if grep -qi 'provenance not checked' "${work}/out-ps-verify-auto.txt"; then
                check ok 'install.ps1 says plainly that provenance was not checked on a mirror'
            else
                check fail 'install.ps1 installed from a mirror without saying provenance was unchecked'
                sed 's/^/      /' "${work}/out-ps-verify-auto.txt" >&2
            fi
        else
            check fail 'install.ps1 refused to install from a mirror, which is a supported override'
            sed 's/^/      /' "${work}/out-ps-verify-auto.txt" >&2
        fi

        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-verify-require' -BaseUrl '${base_url}' -Verify require
        " >"${work}/out-ps-verify-require.txt" 2>&1; then
            check fail 'install.ps1 -Verify require installed from a mirror, where nothing could be verified'
        elif grep -qi 'no attestation to check' "${work}/out-ps-verify-require.txt"; then
            check ok 'install.ps1 -Verify require refuses a mirror by name'
        else
            check fail 'the require refusal failed for the wrong reason'
            sed 's/^/      /' "${work}/out-ps-verify-require.txt" >&2
        fi

        if "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Stop'
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-verify-bad' -BaseUrl '${base_url}' -Verify sometimes
        " >"${work}/out-ps-verify-bad.txt" 2>&1; then
            check fail 'install.ps1 accepted an unknown -Verify value'
        elif grep -qi "expected 'auto'" "${work}/out-ps-verify-bad.txt"; then
            check ok 'install.ps1 rejects an unknown -Verify value by name'
        else
            check fail 'the unknown -Verify refusal failed for the wrong reason'
            sed 's/^/      /' "${work}/out-ps-verify-bad.txt" >&2
        fi

        # `off` must say nothing at all. Not a `check ok` on the message but on the *absence* of
        # one: a mode that is documented as skipping the check must not leave an operator
        # wondering whether it ran.
        "$ps" -NoProfile -ExecutionPolicy Bypass -Command "
            \$ErrorActionPreference = 'Continue'
            & '${script_win}' -Version '${tag}' -InstallDir '${work_win}/ps-verify-off' -BaseUrl '${base_url}' -Verify off -ModifyPath 0
        " >"${work}/out-ps-verify-off.txt" 2>&1 || true
        if grep -qi 'provenance' "${work}/out-ps-verify-off.txt"; then
            check fail 'install.ps1 -Verify off still reported on provenance'
        else
            check ok 'install.ps1 -Verify off says nothing about provenance'
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
            & '${script_win}' -InstallDir '${work_win}/ps-pinned' -ModifyPath 0
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
