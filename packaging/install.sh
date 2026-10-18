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
#   VOLUCER_API          the API root used to resolve the newest tag
#   VOLUCER_DOWNLOAD_URL the base the assets are fetched from
#   VOLUCER_FORCE        set to 1 to replace an existing binary
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
        wget -q -O "$_dest" "$_url"
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

case ":${PATH}:" in
    *":${dest_dir}:"*) ;;
    *) note "${dest_dir} is not on PATH; add it, e.g. export PATH=\"${dest_dir}:\$PATH\"" ;;
esac
