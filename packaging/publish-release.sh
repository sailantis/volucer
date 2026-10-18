#!/usr/bin/env bash
#
# Publish the draft release that `release.yml` built, and check the promise it makes.
#
# The draft exists because a tag is a decision and a broken release is public the moment it is not
# one. It becomes a release here, and only here, after the `install` job has run the release's own
# `install.sh` against the release's own archives and compared what it installed with the binary
# the build produced. Before that the assets are a claim; afterwards they are a tested claim.
# Ordering matters rather than being tidy: `releases/latest/download/` resolves nothing for a
# draft, so publishing earlier would serve an unverified binary to whoever typed the README's
# one-liner during the window - which is the exact failure `draft: true` was added to prevent.
#
# A script rather than an inline `run:` block, for the reason the other packaging scripts are
# separate: an inline block can only be exercised by publishing a real release. This one is
# idempotent, so it can be run by hand against an already-published tag, which is how its
# assertions were checked rather than assumed.
#
# Usage: packaging/publish-release.sh [tag]
#
# Inputs, from the environment:
#   GITHUB_REF_NAME    the tag, when it is not the argument (v0.1.0)
#   GITHUB_REPOSITORY  owner/repo (sailantis/volucer)
#   GH_TOKEN           a token that can edit releases
set -euo pipefail

die() {
    printf 'publish: %s\n' "$1" >&2
    exit 1
}

note() {
    printf 'publish: %s\n' "$1" >&2
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

tag="${1:-${GITHUB_REF_NAME:-}}"
[ -n "$tag" ] || die 'no tag: pass one as an argument or set GITHUB_REF_NAME'
repo="${GITHUB_REPOSITORY:-}"
[ -n "$repo" ] || die 'GITHUB_REPOSITORY is not set'

# Resolved before publishing rather than assumed: a tag with no release behind it means the
# `release` job did not get this far, and reporting that is more useful than a PATCH 404.
id="$(gh api "repos/${repo}/releases/tags/${tag}" --jq .id 2>/dev/null)" ||
    die "no release for ${tag}: the release job did not create one"
draft="$(gh api "repos/${repo}/releases/${id}" --jq .draft)" ||
    die "release ${id} for ${tag} could not be read"

if [ "$draft" = true ]; then
    note "publishing ${tag}"
    # `make_latest=legacy` asks GitHub to work out "latest" the way it normally does, from the
    # release's creation date and version, rather than forcing this tag to be it. Forcing is
    # right only while releases are published in increasing version order, and a maintenance
    # release on an older line - `v0.1.1` after `v0.2.0` - would otherwise repoint the installer
    # one-liner at the older version. A release on a maintenance line is better marked
    # `"prerelease": true` (or published with `-f make_latest=false`), which is GitHub's own
    # answer to the same problem.
    gh api -X PATCH "repos/${repo}/releases/${id}" \
        -f draft=false -f make_latest=legacy >/dev/null ||
        die "publishing ${tag} failed"
else
    note "${tag} is already published; checking what it promises"
fi

draft="$(gh api "repos/${repo}/releases/${id}" --jq .draft)" ||
    die "release ${id} for ${tag} could not be read back"
[ "$draft" = false ] || die "${tag} is still a draft"

# ----------------------------------------------------------------- the promise

# No token, deliberately. The one-liner in the README is run by somebody who has no account
# here, and an authenticated fetch would pass for a release that nobody else can download.
#
# Bounded retry rather than a sleep, and for the same reason `install-selftest.sh` polls for a
# listening port: `releases/latest` is a redirect GitHub resolves and caches, so it can lag the
# PATCH above by a moment. A genuine 404 still fails - it just takes a few attempts to say so -
# and asserting identity is not part of this check, so an older release answering is not a false
# pass. No `-f`, unlike `install.sh`: the status code is the thing being asserted here, and `-f`
# would turn a 404 into "fetch failed" without naming the URL or what it answered.
fetch_anonymous() {
    _url=$1
    _dest=$2
    command -v curl >/dev/null 2>&1 || die 'curl is required to check the published URLs'
    _attempt=1
    while :; do
        _code="$(curl -sSL --retry 3 -o "$_dest" -w '%{http_code}' "$_url")" ||
            die "${_url} could not be fetched"
        [ "$_code" = 200 ] && break
        if [ "$_attempt" -ge 10 ]; then
            die "${_url} answered ${_code}"
        fi
        _attempt=$((_attempt + 1))
        sleep 2
    done
    # An upload that matched nothing still leaves a green release, and a 0-byte asset - which
    # this repository has produced once already - answers 200 like a real one.
    [ -s "$_dest" ] || die "${_url} answered 200 with an empty body"
}

work="$(mktemp -d 2>/dev/null || mktemp -d -t volucer)"
trap 'rm -rf "$work"' EXIT INT TERM

base="https://github.com/${repo}/releases"

# The tag's own URLs are what has to exist for this release; the `latest` ones are what the README
# promises. They are not the same assertion: a maintenance release is correctly not "latest", and
# the one-liner still has to work from whichever release is.
fetch_anonymous "${base}/download/${tag}/install.sh" "${work}/tag-install.sh"
fetch_anonymous "${base}/download/${tag}/install.ps1" "${work}/tag-install.ps1"
fetch_anonymous "${base}/latest/download/install.sh" "${work}/latest-install.sh"
fetch_anonymous "${base}/latest/download/install.ps1" "${work}/latest-install.ps1"

# The release carries a copy of the installers, and the copies are what a stranger gets. Comparing
# them with the checkout catches the one thing the `install` job cannot see: an upload that
# attached a different file, or a checkout that is not the commit the tag names. `install.ps1` is
# uploaded with `cp`, so the bytes match what `.gitattributes` puts on disk; it is compared rather
# than hashed because the interesting difference is a wrong file, not a wrong digest.
for pair in "install.sh:tag-install.sh" "install.ps1:tag-install.ps1"; do
    source_file=${pair%%:*}
    published=${pair##*:}
    cmp -s "${here}/${source_file}" "${work}/${published}" || die \
        "the published ${source_file} is not the one in this checkout: a stale checkout, or an upload that attached the wrong file"
done

note "${tag} is published, and its installers are downloadable without a token"