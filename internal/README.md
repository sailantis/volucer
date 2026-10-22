# Internal design journals

This directory holds the project's working notes. **They are not documentation, and
they are not maintained for a reader.** They are kept in the repository because the
source comments cite them by section number — a hazard id in a code comment is a
pointer into one of these files — and because the reasoning behind a decision is
often useful a year later.

If you want to run volucer, read [`../docs/`](../docs/README.md) instead.

| File | What it is |
|---|---|
| [`plan.md`](plan.md) | The original design and phase plan. Hazards are numbered; `§` refers to its sections. |
| [`plugins.md`](plugins.md) | The driver and integration design: key-value storage, locks, jobs, gRPC. |

**Nothing here is a description of the shipped code.** `plan.md` was written before
the server existed and `plugins.md` says so about itself in its opening paragraph. A
statement in these files that contradicts the code is a stale note, not a
specification: the code, [`../docs/`](../docs/README.md) and
[`../docs/worker-protocol.md`](../docs/worker-protocol.md) are what hold.

## Why they are not under `docs/`

`docs/` is published, page by page, as the documentation site. A design journal that
reads as an internal monologue has no place on a page a user is meant to follow, and
publishing one makes every stale note a public claim. Moving them here keeps the
citations in the source working while keeping the published surface honest.

## The numbering

Both files use stable identifiers — `hazard 46`, `§6.4`, `item 39` — because source
comments point at them. Treat the numbering as append-only: when a note is superseded,
say so in place rather than renumbering, or every comment that cites it silently starts
pointing at something else.
