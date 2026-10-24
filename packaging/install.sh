#!/bin/sh
#
# Install the volucer binary on Linux or macOS, with nothing but curl (or wget), tar and
# a SHA-256 tool. Written for `curl | sh`:
#
#     curl -fsSL https://github.com/sailantis/volucer/releases/latest/download/install.sh | sh
#
# A POSIX shell script rather than a second copy of `volucer-cli`, and the reason is the
# point of this file existing at all: `volucer-cli` is a Composer package, so it needs PHP,
# and a Python or Ruby operator has no reason to have PHP installed. This installs the same
# releases by the same two decisions - resolve the tag, then verify the checksum before
# extracting anything - without the runtime.
#
# The ordering is the contract, not an implementation detail:
#
#   1. The archive is fetched to a temporary file, never to its destination, so a failure
#      at any later step leaves an existing binary exactly as it was.
#   2. The checksum is compared *before* anything is extracted. Hashing an untrusted file
#      is cheap; extracting one is not.
#   3. The binary is moved into place last, because that is the only step that changes
#      what the operator has.
#
# Environment overrides, all of which exist so the installer can be tested against a
# fixture release rather than only against GitHub:
#
#   VOLUCER_VERSION      a tag to install (v0.1.0 or 0.1.0) instead of the newest release
#   VOLUCER_INSTALL_DIR  where to put the binary (default: $HOME/.local/bin)
#   VOLUCER_MODIFY_PATH  `1` (default) to add the install directory to PATH in the shell
#                        profile this shell reads; `0` to leave every dotfile alone and only
#                        print what to add by hand
#   VOLUCER_API          the API root used to resolve the newest tag
#   VOLUCER_DOWNLOAD_URL the base the assets are fetched from
#   VOLUCER_FORCE        set to 1 to replace an existing binary
#   VOLUCER_VERIFY       `auto` (default), `require` or `off` - whether to check the build
#                        provenance attestation. `auto` checks when `gh` is installed and
#                        authenticated and the archive came from GitHub; `require` refuses to
#                        install without it; `off` skips the check.
#   VOLUCER_LIBC         `gnu` or `musl`, overriding detection. Not a feature for installing a
#                        foreign binary: it exists so the musl branch, which is the one that
#                        fails silently at the loader rather than at the installer, can be
#                        asserted from a glibc machine. `volucer-cli` has the same override.
#   VOLUCER_ARCH         `x86_64` or `aarch64`, overriding detection. Same reason as
#                        VOLUCER_LIBC: the arm64 refusal has to be reachable from x86_64.
#
set -eu

APP=volucer
REPOSITORY=sailantis/volucer
API=${VOLUCER_API:-https://api.github.com}
DOWNLOAD_URL=${VOLUCER_DOWNLOAD_URL:-https://github.com/${REPOSITORY}/releases/download}

die() {
    printf 'install: %s\n' "$1" >&2
    exit 1
}

note() {
    printf 'install: %s\n' "$1" >&2
}

# ----------------------------------------------------------------- transport

# The protocols curl is allowed to speak, which is a property of the *destination* rather
# than a constant. `--proto '=https'` is what stops a redirect downgrading the download to
# plain text, and it is enforced for the default GitHub origin. When VOLUCER_DOWNLOAD_URL is
# set the operator has named a different origin - a mirror, GitHub Enterprise, an air-gapped
# host on http - and refusing their explicit choice would make the override useless for the
# one case it exists for. So the list follows the scheme of the URL being fetched, and the
# default path is still https and nothing else.
curl_proto() {
    case "$1" in
        https://*) printf '%s' '=https' ;;
        http://*) printf '%s' '=http,https' ;;
        *) printf '%s' '=https' ;;
    esac
}

# wget's half of `curl_proto`, which is not the same problem: wget has no `--proto` at all, and
# `--https-only` - the flag that refuses a redirect to plain text - exists only in GNU wget.
# BusyBox's does not have it, and an installer that died because an appliance's wget did not
# know a flag would be worse than one that fetched over TLS without the redirect guard. So the
# flag is passed where it exists and the scheme of the URL is what decides whether it applies,
# which is the same rule `curl_proto` states.
wget_flags() {
    if [ "$(curl_proto "$1")" = '=https' ] &&
        wget --help 2>&1 | grep -q -- '--https-only'; then
        printf '%s' '--https-only'
    fi
}

# curl first, wget second. BusyBox systems frequently have only wget, and a container
# built FROM scratch has neither - which is why the failure names both rather than
# reporting "command not found" for whichever one was tried.
fetch() {
    _url=$1
    _dest=$2
    if command -v curl >/dev/null 2>&1; then
        # -f so an HTTP error is a failure rather than a body written to the destination:
        # a 404 page saved as `volucer-0.1.0.tar.gz` would otherwise fail at extraction,
        # which names the symptom and not the cause.
        curl -fsSL --retry 3 --proto "$(curl_proto "$_url")" --tlsv1.2 "$_url" -o "$_dest"
    elif command -v wget >/dev/null 2>&1; then
        # Unquoted on purpose: the expansion is empty or exactly one flag, and no word it can
        # produce contains a space or a glob character.
        # shellcheck disable=SC2046
        wget $(wget_flags "$_url") -q -O "$_dest" "$_url"
    else
        die 'neither curl nor wget is installed; one of them is required'
    fi
}

fetch_stdout() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 --proto "$(curl_proto "$1")" --tlsv1.2 "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O - "$1"
    else
        die 'neither curl nor wget is installed; one of them is required'
    fi
}

# ----------------------------------------------------------------- platform

os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)

case "$os" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    *) die "no archive for ${os}. volucer ships Linux, macOS and Windows builds." ;;
esac

case "${VOLUCER_ARCH:-$arch}" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "no archive for ${VOLUCER_ARCH:-$arch}. volucer ships 64-bit x86_64 and arm64 builds." ;;
esac

# musl is not cosmetic: a glibc archive on Alpine downloads, verifies against a real
# checksum, extracts, and then fails at the loader with an error that points nowhere near
# the installer. `/etc/alpine-release` is the backstop, and `ldd` is consulted as a hint
# for other musl distributions.
libc=gnu
if [ "$os" = linux ]; then
    case "${VOLUCER_LIBC:-}" in
        gnu | musl) libc=${VOLUCER_LIBC} ;;
        '')
            if [ -f /etc/alpine-release ]; then
                libc=musl
            elif command -v ldd >/dev/null 2>&1; then
                if ldd --version 2>&1 | grep -qi musl; then
                    libc=musl
                fi
            fi
            ;;
        *)
            die "VOLUCER_LIBC is \"${VOLUCER_LIBC}\"; expected \"gnu\" or \"musl\""
            ;;
    esac
elif [ -n "${VOLUCER_LIBC:-}" ] && [ "${VOLUCER_LIBC}" != gnu ]; then
    die "VOLUCER_LIBC=${VOLUCER_LIBC} makes no sense on ${os}, which has no second libc"
fi

if [ "$os" = darwin ]; then
    target="${arch}-apple-darwin"
elif [ "$libc" = musl ]; then
    # Only x86_64 musl is published. arm64 would have to fall back to the glibc archive,
    # which is the one "helpful" choice guaranteed to be broken, so it is refused by name.
    if [ "$arch" != x86_64 ]; then
        die 'no musl archive for Linux on arm64. Available: x86_64 musl, x86_64 and arm64 glibc.'
    fi
    target="${arch}-unknown-linux-musl"
else
    target="${arch}-unknown-linux-gnu"
fi

if [ "$os" = darwin ]; then
    note "platform darwin/${arch} -> ${target}"
else
    note "platform linux/${arch} (${libc}) -> ${target}"
fi

# ----------------------------------------------------------------- verify mode

# Validated here, before the download, for the reason an argument is validated anywhere: an
# installer that fetches five megabytes and then reports that its own mode was misspelled has
# spent the operator's time to learn something it could have said immediately. The check itself
# lives in `verify_provenance`; this is only its early refusal.
case "${VOLUCER_VERIFY:-auto}" in
    auto | require | off) ;;
    *)
        die "VOLUCER_VERIFY is \"${VOLUCER_VERIFY}\"; expected \"auto\", \"require\" or \"off\""
        ;;
esac

# `require` against an origin that publishes no attestations cannot succeed, and saying so now
# beats downloading an archive first to reach the same conclusion.
if [ "${VOLUCER_VERIFY:-auto}" = require ] &&
    [ "${DOWNLOAD_URL}" != "https://github.com/${REPOSITORY}/releases/download" ]; then
    die "VOLUCER_VERIFY=require, but ${DOWNLOAD_URL} is not the GitHub release origin; there is no attestation to check there"
fi

# Same reason, and one more: this setting is read in the last block of the script, so a typo in
# it would otherwise be reported after the binary was already replaced - and would be missed
# entirely on a host where the directory is already on PATH and that block never runs.
case "${VOLUCER_MODIFY_PATH:-1}" in
    0 | 1) ;;
    *)
        die "VOLUCER_MODIFY_PATH is \"${VOLUCER_MODIFY_PATH}\"; expected \"0\" or \"1\""
        ;;
esac

# ----------------------------------------------------------------- version

# The tag is resolved before the asset name, because the asset name contains the version:
# `volucer-<version>-<target>.tar.gz`. Hardcoding it in a URL is how a link that says
# "latest" silently starts returning 404 after the next release.
version=${VOLUCER_VERSION:-}
if [ -z "$version" ]; then
    body=$(fetch_stdout "${API}/repos/${REPOSITORY}/releases/latest") ||
        die "the releases API could not be reached at ${API}"
    # `"tag_name": "v0.1.0"` - matched rather than JSON-parsed. A shell has no JSON
    # parser, and a one-key extraction that names the key it wants is honest about being
    # an extraction; a "minimal JSON parser" in `sh` would be neither minimal nor correct.
    version=$(printf '%s\n' "$body" |
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
        head -n 1)
    [ -n "$version" ] || die 'the releases API answered with no tag_name; no release to install'
fi

case "$version" in
    v*) ;;
    *) version="v${version}" ;;
esac
numeric=${version#v}

asset="${APP}-${numeric}-${target}.tar.gz"
note "release ${version}"
note "asset   ${asset}"

# ----------------------------------------------------------------- install

dest_dir=${VOLUCER_INSTALL_DIR:-"$HOME/.local/bin"}
final="${dest_dir}/${APP}"

if [ -e "$final" ] && [ "${VOLUCER_FORCE:-}" != 1 ]; then
    die "${final} already exists. Set VOLUCER_FORCE=1 to replace it."
fi

work=$(mktemp -d 2>/dev/null || mktemp -d -t volucer)
trap 'rm -rf "$work"' EXIT INT TERM

archive="${work}/${asset}"
checksum="${work}/${asset}.sha256"

note 'downloading'
fetch "${DOWNLOAD_URL}/${version}/${asset}" "$archive" ||
    die "could not download ${DOWNLOAD_URL}/${version}/${asset}"
fetch "${DOWNLOAD_URL}/${version}/${asset}.sha256" "$checksum" ||
    die "could not download the checksum beside ${asset}; refusing to install unverified"

# `sha256sum` writes `<hex>  <file>`; `shasum -a 256` writes the same. Only the first field
# is taken, so the recorded filename - which is relative to the release, not to this
# temporary directory - cannot make the comparison depend on the working directory.
expected=$(sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p' "$checksum" | head -n 1)
[ -n "$expected" ] || die "${asset}.sha256 does not contain a SHA-256 digest"

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$archive" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$archive" | cut -d' ' -f1)
elif command -v openssl >/dev/null 2>&1; then
    actual=$(openssl dgst -sha256 "$archive" | sed 's/.*= //')
else
    die 'no SHA-256 tool: install sha256sum, shasum or openssl. Refusing to install unverified.'
fi

# Case-folded, because a digest is hex and `shasum` on some systems upper-cases it.
actual=$(printf '%s' "$actual" | tr 'A-F' 'a-f')
expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')

if [ "$actual" != "$expected" ]; then
    die "the checksum does not match. expected ${expected}, got ${actual}. Nothing was installed."
fi
note 'checksum verified'

# ------------------------------------------------------------------ provenance

# The checksum above proves the archive matches a file on *the same server*. It does not prove
# that file came from this repository's release workflow: whoever can replace the archive can
# replace the `.sha256` beside it, and the default download path is `releases/latest/`, which
# moves. What closes that is the build provenance attestation the release workflow writes with
# `actions/attest-build-provenance` - signed by GitHub's OIDC identity and recorded in Sigstore's
# transparency log, so it is verifiable against a trust root the download origin does not control.
#
# `gh attestation verify` does that verification. It needs a `gh` binary and a GitHub token,
# which an installer running under `curl | sh` cannot assume, and it cannot say anything about a
# mirror or an air-gapped host, whose releases were not built by that workflow. So:
#
#   auto (default)  verify when `gh` is present, authenticated and the asset came from the
#                   default GitHub origin; otherwise say plainly what was *not* checked
#   require         verify, or refuse to install. For a deployment that must not take the
#                   weaker guarantee
#   off             do not verify, and do not comment on it
#
# **A failed verification is fatal, not a warning.** The one case where this matters is the one
# where it must stop: an archive that does not match its attestation. `VOLUCER_VERIFY=off` is the
# deliberate way past it, which is a decision an operator makes rather than one a network hiccup
# makes for them.
verify_provenance() {
    _archive=$1
    _mode=${VOLUCER_VERIFY:-auto}

    # Already validated before the download; `off` is the only one that short-circuits.
    case "$_mode" in
        off) return 0 ;;
        auto | require) ;;
    esac

    # Only the GitHub origin has attestations to check. A mirror is not a failure - it is the
    # documented override, and the operator who named it knows where the bytes came from.
    case "${DOWNLOAD_URL}" in
        "https://github.com/${REPOSITORY}/releases/download") ;;
        *)
            [ "$_mode" = require ] &&
                die "VOLUCER_VERIFY=require, but ${DOWNLOAD_URL} is not the GitHub release origin; there is no attestation to check there"
            note "provenance not checked: ${DOWNLOAD_URL} is not the GitHub release origin"
            return 0
            ;;
    esac

    if ! command -v gh >/dev/null 2>&1; then
        [ "$_mode" = require ] &&
            die 'VOLUCER_VERIFY=require, but the GitHub CLI (`gh`) is not installed; it is what verifies the attestation'
        note 'provenance not checked: the GitHub CLI (`gh`) is not installed, so the archive is verified only against the checksum beside it'
        return 0
    fi

    if ! gh auth status >/dev/null 2>&1; then
        [ "$_mode" = require ] &&
            die 'VOLUCER_VERIFY=require, but `gh` is not authenticated; run `gh auth login`'
        note 'provenance not checked: `gh` is not authenticated, so the archive is verified only against the checksum beside it'
        return 0
    fi

    if gh attestation verify "$_archive" --repo "$REPOSITORY" >/dev/null 2>&1; then
        note 'provenance verified'
        return 0
    fi

    # Fatal, and the wording names both causes rather than asserting the worse one: a digest with
    # no attestation is what a replaced archive looks like, and a failed API call looks the same
    # from here because `gh` reports both as a non-zero exit. Either way the check did not pass,
    # and installing on a check that did not pass is the thing this step exists to prevent.
    die "the build provenance of ${asset} could not be verified: the archive is not attested as built by this repository's release workflow, or the attestation could not be fetched. Nothing was installed. (VOLUCER_VERIFY=off skips this check.)"
}

verify_provenance "$archive"

# Extracted here and not beside the destination: `tar` would write into whatever directory
# the operator happened to be standing in, which `stage-archive.sh` deliberately avoids by
# packing one top-level directory.
tar -xzf "$archive" -C "$work" || die "could not extract ${asset}"

staged="${work}/${APP}-${numeric}-${target}/${APP}"
[ -f "$staged" ] || die "the archive does not contain ${APP}-${numeric}-${target}/${APP}"

mkdir -p "$dest_dir" || die "could not create ${dest_dir}"
chmod +x "$staged"
mv "$staged" "$final" || die "could not move the binary to ${final}"

note "installed ${final}"
"$final" version 2>/dev/null | head -n 1 >&2 || true

# ----------------------------------------------------------------- path

# The operator's next action is opening a new shell, so the file used is the one an
# *interactive* shell of theirs reads rather than the login-only one. zsh reads `.zshrc` in a
# login shell too, while `.zprofile` is skipped by a non-login terminal - and a terminal that
# is not a login shell is the common case on Linux. bash reads `.bashrc` in every interactive
# non-login shell, and falls back to `.profile` on a host that has no `.bashrc`, which is what
# a login bash reads.
#
# A `curl | sh` cannot ask, which is why this is a default and not a prompt. What makes it
# safe instead of silent is the rest of this block: it is opt-out (`VOLUCER_MODIFY_PATH=0`),
# it does nothing when the file already adds the directory - Debian and Ubuntu ship exactly
# that line in the stock `~/.profile` - it resolves a symlinked profile before writing, so a
# dotfiles repository keeps owning the file, and it prints the file, the exact line and how to
# take it back out.
path_profile() {
    case "${SHELL:-}" in
        */zsh) printf '%s' "${ZDOTDIR:-$HOME}/.zshrc" ;;
        */fish) printf '%s' "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
        */bash)
            if [ -f "${HOME}/.bashrc" ]; then
                printf '%s' "${HOME}/.bashrc"
            else
                printf '%s' "${HOME}/.profile"
            fi
            ;;
        *) printf '%s' "${HOME}/.profile" ;;
    esac
}

# `readlink -f` is GNU-only; macOS has neither `-f` nor the same behaviour. The loop is the
# portable form, and the depth limit is what stops a symlink cycle from hanging an installer.
resolve_symlink() {
    _path=$1
    _hops=0
    while [ -L "$_path" ] && [ "$_hops" -lt 40 ]; do
        _link=$(readlink "$_path") || return 1
        case "$_link" in
            /*) _path=$_link ;;
            *) _path="$(dirname "$_path")/${_link}" ;;
        esac
        _hops=$((_hops + 1))
    done
    printf '%s' "$_path"
}

add_to_path() {
    case "${VOLUCER_MODIFY_PATH:-1}" in
        1) ;;
        0)
            note "${dest_dir} is not on PATH; VOLUCER_MODIFY_PATH=0, so no file was changed"
            note "add it by hand, e.g. export PATH=\"${dest_dir}:\$PATH\""
            return 0
            ;;
    esac

    if [ -z "${HOME:-}" ]; then
        note "${dest_dir} is not on PATH, and HOME is unset, so there is no profile to edit"
        return 0
    fi

    profile=$(path_profile)

    # A dotfiles repository usually owns the profile through a symlink. Appending to the link
    # would replace it with a regular file and lose the repository's copy - quietly, and until
    # the next `chezmoi apply` or `stow` puts it back and drops the line again.
    if [ -e "$profile" ]; then
        if resolved=$(resolve_symlink "$profile"); then
            profile=$resolved
        fi
    fi

    case "${SHELL:-}" in
        */fish) line="set -gx PATH \"${dest_dir}\" \$PATH" ;;
        *) line="export PATH=\"${dest_dir}:\$PATH\"" ;;
    esac

    marker='# added by the volucer installer'
    below_home=''
    case "$dest_dir" in
        "$HOME"/*) below_home=${dest_dir#"$HOME"} ;;
    esac

    # Comments are excluded from every spelling below. A file that merely mentions the
    # directory - a commented-out block, a note to self - does not put it on PATH, and treating
    # it as though it did would answer "open a new shell" to an operator whose new shell will
    # not have the binary either.
    profile_sets_path() {
        [ -f "$profile" ] || return 1
        _lines=$(grep -v '^[[:space:]]*#' "$profile" 2>/dev/null) || return 1
        printf '%s\n' "$_lines" | grep -Fq -- "$dest_dir" && return 0
        if [ -n "$below_home" ]; then
            # A stock Debian `~/.profile` spells the default as `$HOME/.local/bin`, and a
            # hand-written file often spells it `~/.local/bin`; neither is the expanded path
            # this installer would write, and missing them duplicates a line that was there.
            printf '%s\n' "$_lines" | grep -Fq -- "\$HOME${below_home}" && return 0
            printf '%s\n' "$_lines" | grep -Fq -- "~${below_home}" && return 0
        fi
        return 1
    }

    already=0
    if [ -f "$profile" ] && grep -Fq -- "$marker" "$profile" 2>/dev/null; then
        already=1
    elif profile_sets_path; then
        already=1
    fi

    if [ "$already" = 1 ]; then
        note "${profile} already adds ${dest_dir}; open a new shell to pick it up"
        return 0
    fi

    mkdir -p "$(dirname "$profile")" 2>/dev/null || true
    if printf '\n%s\n%s\n' "$marker" "$line" >>"$profile" 2>/dev/null; then
        note "${dest_dir} was added to PATH in ${profile}"
        note "    ${line}"
        note "to undo it, delete that line and the comment above it, then open a new shell"
    else
        note "${dest_dir} is not on PATH and ${profile} could not be written"
        note "add it by hand: ${line}"
    fi
}

case ":${PATH}:" in
    *":${dest_dir}:"*) ;;
    *) add_to_path ;;
esac
