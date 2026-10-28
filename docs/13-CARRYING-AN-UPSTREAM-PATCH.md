# Carrying an upstream patch

Volucer depends on `h3` for HTTP/3, and `h3` is 0.0.8 — pre-1.0 and explicitly experimental. This
page records the policy for the day a bug in it has to be fixed before upstream releases a fix,
so that the decision is already made when it is needed rather than at the moment of pressure.

## The rule: upstream first, always

If a bug in `h3` blocks this project, the order is fixed:

1. **Open the pull request against `hyperium/h3` first.** Not second, not "after it works". A fix
   living only in a fork is a fix nobody else gets, and a fork is easier to forget than a PR is to
   answer.
2. **Fork only once there is a diff to carry.** A `[patch]` entry pointing at a fork with no diff
   is a false statement in a manifest: it makes every build depend on GitHub, makes `--locked`
   load-bearing, and adds a `cargo-deny` source exemption — all for a change of zero.
3. **Delete the patch on adoption. Do not merge the fork.** When a release carries the fix, the
   `[patch]` line goes and the version bump arrives in the same commit. A fork that is kept "in
   case" is drift, and drift nobody watches is a fork nobody tests.
4. **Never pin a contributor's PR branch.** A PR branch can be force-pushed or rebased away, and a
   build that depended on one would fail at a moment nobody chose. Fork the repository at a commit
   *this project* tagged.

The fork is `sailantis/hyperium-h3`, and its `FORK.md` records the same four rules from the other
side — the side that has to keep the fork usable.

## What the patch actually looks like

At the workspace root, and only while the fork carries a diff:

```toml
[patch.crates-io]
h3 = { git = "https://github.com/sailantis/hyperium-h3", tag = "volucer/<topic>" }
```

**A tag, never a branch.** An upstream rebase cannot move a tag, so what a build compiles is what
was reviewed. The tag is annotated and points at a commit on the fork's `master` with the carried
change on top.

`Cargo.lock` is committed with the rev it resolved to, and every build of the *binary* uses
`--locked`: a release workflow that resolved a different commit than CI tested would ship
untested code with a version number that says otherwise.

## The limitation that makes configuration the real defence

`[patch]` applies when **this repository** is the root of the build. That covers `cargo install
volucer`, the release artifacts and CI.

It does **not** apply to anyone depending on `volucer-http` as a *library* crate: they would get
the vulnerable `h3` from crates.io, because the patch is a statement in this workspace's manifest
and their workspace is the root of their build.

So a patch protects our binary and nothing else. What protects every consumer is the
**configuration**: `[server.http3].max_field_section_size` bounds the header section regardless of
which `h3` is linked, and the default is what a deployment gets without writing anything. That is
why the mitigation in `crates/volucer-core/src/config/http3.rs` exists even though the advisory is
"somebody else's to fix", and why it is tested rather than merely set.

## `cargo-deny` and the fork

`deny.toml` already admits the fork's git source:

```toml
[sources]
allow-git = ["https://github.com/sailantis/hyperium-h3"]
```

That line was written before the first patch existed, deliberately: the alternative is a CI failure
on the day the patch is added, whose message says nothing about why the source is legitimate.

An `unknown-git = "deny"` setting with one named exception is the shape to keep. Widening it to
`allow` would accept any git source in the graph, which is a supply-chain decision nobody made.

## Advisories, and the one that is already recorded

`h3` 0.0.8 carries hyperium/h3#379 — an unbounded `max_field_section_size`, which is a remotely
triggerable memory amplification — and no release carries the fix. `deny.toml` ignores nothing for
it, because the vulnerability check does not have an advisory ID for it: it is tracked as a GitHub
issue rather than a RUSTSEC entry. The mitigation is the configuration described above, and it is
pinned by a test that asserts an oversized section is answered 431 *and* that the listener serves
the next request. If that test is deleted, the mitigation is a comment.

When upstream ships the fix, the sequence is: bump `h3`, delete anything left of the patch, and
delete the corresponding paragraph here and in `FORK.md`.

## Where this is recorded

- `sailantis/hyperium-h3/FORK.md` — the fork's own rules and its baseline tag.
- This page — the consumer's side.
- `deny.toml` — the source exemption, and the reason beside it.
- `CHANGELOG.md` — the advisory, in the `Security` section of the release that added HTTP/3.
