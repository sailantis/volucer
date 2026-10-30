# Deploying

This guide is the deployment story end to end: how to ship a new version of your application
without dropping a request, and what to do when the roll cannot finish in one pass.

It is deliberately narrower than [Operations](./11-OPERATIONS.md), which covers every setting a
deployment touches — TLS, reverse proxying, `systemd`, containers, logs. This document is about
one operation, done repeatedly, under load.

## The three verbs, again

A deployment that conflates these three either does not deploy or drops requests. They are
different operations and they are triggered differently:

| Verb                 | Signal               | What it does                                              | Drops a request? |
| -------------------- | -------------------- | --------------------------------------------------------- | ---------------- |
| Reload configuration | `SIGHUP`             | Re-reads the file and applies the reloadable settings      | No               |
| Roll application code| `SIGUSR2`            | Replaces the worker processes, keeping every address       | No               |
| Stop                 | `SIGTERM` / `SIGINT` | Stops accepting, finishes in-flight requests, then exits    | No, up to the drain deadline |

The distinction that matters most, and the one people get wrong:

> **`SIGHUP` does not deploy application code.**

A persistent worker — `worker` and `spawn_cgi` alike — loads your code once, when the process
boots. Reloading configuration swaps tables inside the server and leaves every PHP process
exactly as it was, which is the entire point of a persistent worker. A `SIGHUP` after `git pull`
deploys nothing.

## What a roll replaces, and what it does not

A roll replaces the **processes**. It does not change the server's own configuration:

| A roll changes                          | A roll leaves alone                                  |
| --------------------------------------- | ---------------------------------------------------- |
| The PHP processes behind each slot      | Their count, their command, their environment        |
| The code each process has loaded        | The listeners, the sites, the routes                 |
| Which processes serve, and when         | Every request already being served                   |

If the deploy changes `command`, `workers` or `env`, that is a **restart**, not a roll. See
[Operations](./11-OPERATIONS.md#what-a-reload-changes) for the full restart-only list.

## The roll, step by step

### 1. Get the new code onto disk

Nothing about a roll fetches or builds anything. `git pull`, a compiled image layer, a
`rsync` — however your code arrives, it has to be on disk *before* the sweep runs, at the
path the configured `command` already points at.

An atomic swap is worth the effort here: a worker booting in the middle of a half-written
directory loads half an application. Write the new release to a fresh directory and move a
symlink, rather than rewriting files in place.

### 2. Grant the route, if you want to use it

The signal needs nothing. The route does — a route that does not exist until the file grants
it is the same protection `[status]` uses for every other route:

```toml
[status]
listen = "127.0.0.1:2114"

[status.workers]
allow = ["workers.restart"]
```

`workers.restart` is the only grant the sweep needs: the command posts to
`/workers/restart` and reads the report the sweep answers with, so granting `workers.list`
as well is not required. Add it only if you also want `GET /workers` for a person or a
dashboard.

Reload the configuration (`SIGHUP`) if you have just added the grant. Windows has no `SIGHUP`,
so restart the server there — the grant is read at startup on both platforms. A server that
does not grant it answers `404`, which is deliberate: the route does not exist rather than
existing and refusing, so an ungranted route is not a probe a stranger can discover.

### 3. Roll

```sh
volucer workers restart --config /etc/volucer/volucer.toml
```

or, for a supervisor that can send a signal:

```sh
kill -USR2 "$(pidof volucer)"
```

Both are the same sweep. The command calls the route, and the route and the signal share one
implementation, so they cannot disagree about what an idle slot is.

### 4. Sweep again if the roll did not complete

**This is the part a deploy script has to handle, and it is the honest trade of "never drop a
request".**

The sweep replaces only slots that are **not serving a request**. On a deployment under
continuous load there may be no idle slot at the moment the sweep runs, and then it replaces
nothing and says so:

```console
$ volucer workers restart --config /etc/volucer/volucer.toml
app: 0 slot(s) replaced; 2 busy and left for the next sweep
$ echo $?
1
```

Nothing is broken and no request was lost — both busy slots finished serving. The roll simply
has not happened yet, so **sweep again**.

`--json` prints the same report as data, and the exit status is the machine-readable signal:

| Exit | Meaning                                                             |
| ---- | ------------------------------------------------------------------- |
| `0`  | Every slot that can be rolled was rolled                             |
| `1`  | Accepted, but incomplete: at least one slot still runs old code      |

**Exit `1` has two causes, and they need different responses.** The summary names it:

```console
# busy: an unfinished roll, and another sweep finishes it
app: 0 slot(s) replaced; 2 busy and left for the next sweep

# failed: the application does not start, and sweeping again will not fix it
app: 0 slot(s) replaced; 1 FAILED to restart
```

The same distinction is in the JSON, which a script should read rather than matching prose:

```console
# busy
{"backends":[{"backend":"app","failed":[],"replaced":[],"skipped_busy":[0,1],"unmanaged":null}],"complete":false}

# failed
{"backends":[{"backend":"app","failed":[0],"replaced":[],"skipped_busy":[],"unmanaged":null}],"complete":false}
```

So a loop has to look at **which** cause, and it has to be bounded. Read the `failed` list from
the JSON rather than matching the summary's prose, so a wording change cannot silently turn a
failure into a retry — and if `failed` is non-empty, stop rather than sweep again, because the
new code does not start:

```sh
# Sweep while slots are merely busy; stop on success, on a failure, or after the bound.
attempt=0
while [ "$attempt" -lt 30 ]; do
  attempt=$((attempt + 1))

  if report=$(volucer workers restart --config /etc/volucer/volucer.toml --json); then
    exit 0                                   # complete: nothing left to roll
  fi

  # A non-empty `failed` list means a replacement could not boot. Sweeping again cannot fix an
  # application that does not start, so this is a hard stop rather than a retry.
  case "$report" in
    *'"failed":[]'*) sleep 1 ;;               # only busy slots: an unfinished roll
    *)
      echo "the roll failed, the new code does not boot: $report" >&2
      exit 1
      ;;
  esac
done
echo "the roll did not complete in $attempt sweeps; the slots never went idle" >&2
exit 1
```

The `case` matches the serialized report rather than parsing it, which is coarse but exact
enough: `RespawnReport` serializes `failed` as `[]` or a list of indices, so `"failed":[]` is
present only when no replacement failed. If your pipeline would rather parse, `jq` works on the
same field — the JSON shape is the one shown above.

A slot whose replacement failed is left **empty**, which is why a request to it answers `502`
rather than the previous code's response: the roll did not silently keep the old version running.
That is deliberate — a deploy that cannot start should be visible, not absorbed.

The bound matters for the busy case too: a deployment whose workers never go idle will never
complete a roll, and the fix there is `max_requests` or a shorter `request_timeout_ms` rather
than a longer loop.

### What each backend kind does

| Backend              | A sweep…                                                                      |
| -------------------- | ----------------------------------------------------------------------------- |
| `worker`             | replaces each idle slot's process, in place                                    |
| `spawn_cgi`          | replaces each idle slot's process **on its own address**                       |
| `attach`             | does nothing — the supervisor that started those processes owns them           |
| `external` (php-fpm) | does nothing — reload php-fpm instead (`SIGHUP`/`USR2` to its own master)      |
| `upstream`           | restarts the origin **if** volucer supervises it; in-flight requests are lost  |

`attach` and `external` are reported as *unmanaged*, with the reason, rather than counted as
success. A `spawn_cgi` slot keeps its address across a replacement, which is why the sweep can
run without closing a listener.

An `upstream` origin is the one kind that cannot be rolled without losing requests: it serves
many at once and has no idle state, so the restart is announced and the origin's readiness gate
is waited out before the sweep reports success. An `upstream` backend with no `command` is
proxy-only — volucer did not start that origin — so it is reported as unmanaged and its deploy
is the origin's own.

## Under Kubernetes

Kubernetes has no signals, so use the route. `preStop` gives the endpoint removal time to
propagate before the listener closes, so a request routed to a leaving pod is not refused:

```yaml
lifecycle:
  preStop:
    exec:
      # The pod is being removed from the endpoint list; this window is for the
      # load balancer to notice, before the listener closes.
      command: ["sh", "-c", "sleep 5"]
terminationGracePeriodSeconds: 40
```

`terminationGracePeriodSeconds` must exceed `[server] shutdown_timeout_ms` **plus** the `preStop`
sleep, or the `SIGKILL` arrives mid-drain and produces exactly the truncation the drain exists to
avoid. The same relationship holds for `systemd`'s `TimeoutStopSec` and Docker's
`stop_grace_period`, where the drain has no separate `preStop` stage.

For the roll itself, run the same command the other platforms run, from an init container or a
CI step with cluster access:

```sh
kubectl exec deploy/app -- volucer workers restart --config /etc/volucer/volucer.toml
```

Because a sweep may be incomplete, wrap the loop above in `kubectl exec` rather than writing a
second one — the failure and busy cases are the same, and only the transport differs:

```sh
attempt=0
while [ "$attempt" -lt 30 ]; do
  attempt=$((attempt + 1))

  if report=$(kubectl exec deploy/app -- volucer workers restart --config /etc/volucer/volucer.toml --json); then
    exit 0
  fi

  case "$report" in
    *'"failed":[]'*) sleep 1 ;;
    *)
      echo "the roll failed, the new code does not boot: $report" >&2
      exit 1
      ;;
  esac
done
echo "the roll did not complete in $attempt sweeps" >&2
exit 1
```

A rollout that recreates pods does not need this — a new pod boots new code by definition. This
matters for the case the sweep exists for: **new code, same process**, which is what makes a
persistent worker fast in the first place.

## Under Docker

A container runs one volucer process, so the roll is `docker exec` against a running container:

```sh
docker exec app volucer workers restart --config /etc/volucer/volucer.toml
```

The container has to have the binary on `PATH` and the configuration readable at that path. The
published images satisfy both: the binary is `/usr/local/bin/volucer`, and the image's own
`CMD` already runs `serve --config /etc/volucer/volucer.toml`, so the mount an operator writes
is the same path this command reads:

```sh
docker run -v ./volucer.toml:/etc/volucer/volucer.toml:ro … volucer
```

If a deploy replaces the container instead, the roll is unnecessary: stop the old container
with a grace period longer than `shutdown_timeout_ms`, and let it drain.

## Ordering, for a deploy that changes both

When a release changes the application *and* the configuration, the order matters:

1. Write the new code.
2. Reload configuration (`SIGHUP`) if the configuration changed in a reloadable way.
3. Roll the workers (`SIGUSR2`), sweeping until complete.
4. Restart only if the configuration changed something in the restart-only list.

Doing 3 before 2 is usually wrong: new code that expects a new site or route is served by a
server that has not read it yet.

## What to watch

| Symptom                              | Cause                                                                         |
| ------------------------------------ | ----------------------------------------------------------------------------- |
| The roll never completes             | Every slot stays busy. Bound it with `max_requests`.                           |
| A `502` during a roll                | The application failed to boot: the slot was replaced and the replacement died. |
| A `503` during a roll                | Every slot is busy and `max_wait` expired. Capacity, not a broken deploy.      |
| A truncated response at deploy time  | The supervisor's grace period is shorter than the drain.                      |
| New code not live after `SIGHUP`     | Expected — `SIGHUP` does not deploy code.                                     |

The two error codes are worth telling apart, and the distinction is deliberate rather than
incidental: a pool that could not take the request at all is `503` because nothing was attempted
and a retry may work, while a backend that was reached and failed is `502`. A deploy that
produces a burst of `503`s is a capacity question; one that produces `502`s is looking at the
application.

The sweep logs one line per backend with what it replaced, skipped and failed, so a deploy log
answers "did this roll?" without a second command.
