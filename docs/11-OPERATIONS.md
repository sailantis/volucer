# Operations

**Reloading, running behind nginx, systemd, containers, and the release process.**

## Reloading

Send `SIGHUP` and the configuration file is re-read and applied. **Unix only** — Windows has
no `SIGHUP` and no equivalent, so a Windows deployment restarts.

```console
kill -HUP $(pidof volucer)      # or: systemctl reload volucer
```

The reloadable state is one value behind one pointer, and replacing it affects the **next**
request. An in-flight request keeps the configuration it started under — the only ordering
that can be explained to an operator, and the reason there is one pointer rather than one per
setting: three separate swaps would let a request take a new site table with an old routing
map, which would look like a site serving the wrong document root rather than like a race.

The same signal reaches **every** front-end: an HTTP listener and a FastCGI listener share
one site table, so a reload cannot take effect on `:80` and leave nginx reaching the old pool.

**A reload never stops the server.** `SIGHUP` reloads and `SIGTERM` stops; they are handled
per signal, so a `kill -HUP` can never take the server down by accident.

### What a reload changes

**Reloadable:**

- `[server.tls]` and `[sites.*.tls]` certificates and keys
- `[sites.*]` `hosts`, `document_root`, `front_controller`, `script_extensions`,
  `serve_only`, `default_backend` and `routes`
- `[server.static_files]`, including turning the layer on or off
- `redirect_to_https`
- A backend's `grpc_services` and `grpc_serve_any` — a widened grant admits the next call,
  on the same connection
- A backend's `grpc_proto`, which is re-read: the `.proto` files are parsed again, so a `service`
  added to one of them is granted on the next call. The files are **not watched** — the change
  takes effect when you signal, which is also what makes a half-written file harmless
- `[server.rate_limit]`, with every tracked caller's bucket carried across when the `key` and
  `header` are unchanged
- `[status] allow_worker_reset`, in both directions

**Restart-only, and reported on every reload** rather than left to be discovered — a setting
the implementation cannot honour should be *stated*, or the operator finds out from
behaviour:

| Setting | Why it cannot be reloaded |
|---|---|
| `http`, `https`, `fastcgi` | A bound socket cannot move |
| Backends, their commands and worker counts | Those are running processes with live pipes |
| `[server.env]` and a backend's `env` | They are the environment of processes already running; a replacement worker would get the new value and its siblings would not |
| `[server.tls]` `min_version` and `alpn` | They are baked into the `rustls` server config the acceptor holds |
| `[server.proxy]` | Whose forwarded headers are believed is a statement about this deployment's topology |
| `[server.access_log]` path, format, `request_id_header` | Chosen at startup. **The log file is reopened on every `SIGHUP` regardless** |
| `[status]` address | The listener is bound |

### A reload is whole-or-nothing

A file that does not parse, or does not validate, is refused with a logged reason and the
previous configuration keeps serving. There is no way to apply half of one — including when
the bad file names settings the server *can* apply.

**A `.proto` file that `grpc_proto` names is part of that same "whole"**, even though it is not
`volucer.toml`. It is read while the configuration is loaded, so an unreadable `.proto`, or one
whose set resolves to no service at all, refuses the reload entirely rather than installing a
backend granted nothing.

There are two tables — the sites and the certificates — and each is whole-or-nothing **on its
own**, applied site table first and certificates after. That ordering is deliberate: a
certificate that cannot be read then leaves a valid site change in effect instead of refusing
both.

**With `[server.acme]`, an issued certificate is carried across a reload** rather than
rebuilt from the configuration, and the reload **merges the two populations**: the table is
rebuilt from the file and every certificate that was issued is re-registered over it, so a
renewed PEM is read *and* an issued certificate stays in service. A name the configuration
says nothing about keeps the certificate it had. If you are migrating a deployment *away*
from ACME, remove `[server.acme]` and restart.

## Behind nginx

You do not need nginx — volucer serves static files and speaks HTTP itself. But if you have
one, use the FastCGI listener:

```toml
[server]
fastcgi = "127.0.0.1:9080"
document_root = "/srv/app/public"
```

```nginx
server {
    listen 80;
    server_name example.com;
    root /srv/app/public;
    index index.php;

    location / {
        try_files $uri /index.php$is_args$args;
    }

    location ~ ^/index\.php(/|$) {
        fastcgi_pass 127.0.0.1:9080;
        fastcgi_split_path_info ^(.+\.php)(/.*)$;
        include fastcgi_params;
        fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;
        fastcgi_param PATH_INFO $fastcgi_path_info;
    }
}
```

**`try_files` has a trap worth knowing.** It treats every argument except the last as a
*file test*, so `/index.php` is checked on disk, found, and served **as a static file** —
nginx hands out the PHP source, unparsed, with a `200`. The last argument is the fallback,
not the first.

The obvious spelling of a front controller is:

```nginx
location / { try_files /index.php =404; }
location = /index.php { fastcgi_pass ...; }
```

and it is wrong for that reason. The shape above — one `try_files` whose fallback is the
script, and a `location` matching the script — does not have the trap.

If you use nginx, tell volucer who the real client is, or the access log records nginx
instead of the client and the rate limiter throttles nginx:

```toml
[server.proxy]
trusted_subnets = ["127.0.0.1"]
trusted_headers = ["X-Forwarded-For"]
```

## systemd

The unit that ships with volucer:

```ini
[Unit]
Description=volucer PHP application server
Documentation=https://github.com/sailantis/volucer
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/volucer serve --config /etc/volucer/volucer.toml
ExecReload=/bin/kill -HUP $MAINPID

KillSignal=SIGTERM
TimeoutStopSec=30

Restart=on-failure
RestartSec=1

User=volucer
Group=volucer
StateDirectory=volucer
WorkingDirectory=/var/lib/volucer

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/volucer

[Install]
WantedBy=multi-user.target
```

`ExecReload` is the reason this unit exists at all: `SIGHUP` re-reads the configuration and
applies it whole, or refuses it and keeps serving the previous one. Without a unit there is
nothing to wire the signal to.

**A unit can also carry the configuration instead of a file**, which is one thing fewer to
mount and one thing fewer to read from disk:

```ini
ExecStart=/usr/local/bin/volucer serve \
    --listen 127.0.0.1:8080 \
    -o server.document_root=/srv/public \
    -o server.front_controller=index.php \
    -o backends.app.kind=worker \
    -o "backends.app.command=php /srv/public/worker.php" \
    -o backends.app.workers=0
```

The overrides are replayed by `SIGHUP` exactly as they were given, so a reload never installs a
configuration nobody wrote. Without a file there is nothing new to read, so a reload re-applies
the same command line — which is what a `logrotate` postrotate hook needs, and which still
refreshes the certificate table. A changed `ExecStart` takes effect when the service is restarted:
a flag is not a file, so there is nothing for the signal to pick up.

`SIGTERM` drains: the listeners stop, in-flight requests finish, and the pool's PHP children
are killed with their parent rather than orphaned.

The ACME cache lives under the state directory, so `[server.acme] cache` and the unit's
`StateDirectory` have to agree. The pair the unit ships with is:

```toml
[server.acme]
cache = "/var/lib/volucer/acme-cache"
```

### Secrets and environment

**volucer loads no `.env` file of its own**, and that is deliberate: `EnvironmentFile=` here,
`docker run --env-file` and Compose's `env_file:` already do it, and a second loader would be a
second precedence rule and a second path-resolution rule for one job. What volucer does is read
the environment it is given, in the file, through `${...}`:

```ini
# the unit
Environment=APP_ENV=production
EnvironmentFile=/etc/volucer/secrets.env
```

```toml
# the configuration
[server]
http = "127.0.0.1:${PORT:-8080}"

[backends.app]
kind = "worker"
command = "php worker.php"
workers = ${WORKERS:-0}
```

The unit's environment takes precedence over `EnvironmentFile=`, and both are visible to the
expansion. Values are then handed to the application through `[server.env]` (shared) or a
backend's own `env` (one table), and `config.describe` reports the names with the values
elided — so a credential supplied this way never travels to a worker.

`volucer check` performs the same expansion, so a missing variable is reported by `check` before
`serve` is ever restarted onto it.

## Health checks

The `[status]` listener answers two probes, and the difference between them is the one an
orchestrator acts on:

```toml
[status]
address = "127.0.0.1:2114"
```

```console
volucer health --url http://127.0.0.1:2114/health   # is the process running?
volucer health --url http://127.0.0.1:2114/ready    # should traffic be sent here?
```

`curl -sf` does the same thing against the same URLs — `volucer health` exists because a
`scratch` container has no `curl` to run it with, not because the endpoints need a client.
See [the Docker section](#health-checks-in-the-image) for that case.

**`/health` answers 200 while the process is running**, even if nothing is ready yet. It is
the answer to "should this container be restarted", and restarting a server that is merely
still booting turns a slow start into a crash loop.

**`/ready` answers the configured `unavailable_status_code` — `503` by default — until every
service reports ready.** It is the answer to "should traffic be sent here", and a probe that
returned 200 while the pools were still booting is how a rolling deploy sends traffic to a
server that answers 502.

Every other path is a 404, on both, so neither endpoint becomes a second way to ask whether
the server is up.

Bind it to loopback or a private interface and reach it through your orchestrator's network.

`allow_worker_reset` adds the worker-control routes to the same listener. Read
[Observability](06-OBSERVABILITY.md#the-worker-control-routes) before turning it on — there
is no authentication on those routes.

## Docker

There are two ways to get an image: pull a published one, or build it from a checkout.

### Pulling a published image

Every release publishes both images to GitHub Container Registry, for `linux/amd64` and
`linux/arm64`:

```console
docker pull ghcr.io/sailantis/volucer:latest          # musl, scratch-based: the recommended one
docker pull ghcr.io/sailantis/volucer:latest-glibc    # Debian-based, if you need a libc
```

The `musl` image is the default — it has no libc dependency and is smaller, which is what a
`scratch` base allows. **`glibc` always carries a suffix**, so a bare pull is never the base
image by accident:

| Tag | Image |
|---|---|
| `0.2.0`, `0.2`, `latest` | musl, `scratch` base |
| `0.2.0-glibc`, `0.2-glibc`, `latest-glibc` | glibc, `debian:bookworm-slim` |

`latest` moves only for a final release: a pre-release such as `v0.3.0-rc.1` publishes
`0.3.0` and `0.3` and leaves `latest` where it is, because `latest` is what a bare
`docker pull` installs. Pin `0.2.0` rather than `latest` in anything that must not change
under it.

The package has to be made public once, after the first release publishes it — see
[Publishing](#publishing) — after which no `docker login` is needed to pull it.

### Building it yourself

`packaging/docker/Dockerfile` builds both images from the one crate, measured at about
3m45s for `musl` and 1m40s for `glibc` on a 4-vCPU machine with a cold Cargo cache:

```console
docker build -f packaging/docker/Dockerfile --target musl -t volucer .           # the host's architecture
docker build -f packaging/docker/Dockerfile --target glibc -t volucer:glibc .
```

`-f` is required: the Dockerfile is in `packaging/docker/`, not at the repository root, while the
build context has to be the root because `COPY . .` copies the whole cargo workspace.

`--target musl` resolves the Rust target from `--platform`, so
`docker build -f packaging/docker/Dockerfile --platform linux/arm64 --target musl` produces an
`aarch64` binary rather than a mislabelled `x86_64` one. An architecture with no musl target is a
build failure, not a fallback.

The two images are 23 MB (`musl`, `scratch`) and 149 MB (`glibc`, `debian:bookworm-slim`).

There is no archive to fetch and nothing to verify by hand, because the binary is built into
the image. `.dockerignore` keeps `target/` and `.git/` out of the build context, which they
would otherwise dominate: a checkout of this repository has a `target/` directory in the tens of
gigabytes, and every byte of it would be uploaded and written into a layer before the compiler
started.

### Running it

Both images have the same contract, so the same invocation works for either — a published image
name drops in for the local one:

```console
docker run --rm \
    -p 8080:8080 -p 9080:9080 \
    -v "$PWD/volucer.toml:/etc/volucer/volucer.toml:ro" \
    -v "$PWD/public:/srv/public:ro" \
    ghcr.io/sailantis/volucer:latest
```

**The image can also be run with no configuration file at all**, which is the shape that needs
nothing mounted and no entrypoint script to generate one:

```console
docker run --rm -p 8080:8080 -p 9080:9080 -v "$PWD/public:/srv/public:ro" \
    ghcr.io/sailantis/volucer:latest \
    serve --listen 0.0.0.0:8080 --document-root /srv/public \
        -o server.front_controller=index.php \
        -o backends.app.kind=worker \
        -o "backends.app.command=php /srv/public/worker.php" \
        -o status.address=0.0.0.0:9080
```

Note the subcommand: `ENTRYPOINT` is the binary, so arguments after the image name replace the
default `serve --config /etc/volucer/volucer.toml` and `serve` has to be named explicitly.

The `HEALTHCHECK` reads `[status]` from a file, so a fileless container needs the probe pointed
at the URL instead: `HEALTHCHECK CMD ["/volucer", "health", "--url",
"http://127.0.0.1:9080/health"]`. See [Health checks](#health-checks-in-the-image) below.

The configuration has to do two things a host deployment does not, and neither is optional:

```toml
[server]
http = "0.0.0.0:8080"          # not 127.0.0.1: the port has to be reachable from outside
document_root = "/srv/public"

[status]
address = "0.0.0.0:9080"       # what `docker exec ... health` probes, and what EXPOSE names
```

A `127.0.0.1` listener is reachable only from inside the container's network namespace, so
`-p 8080:8080` publishes a port nothing answers on. The failure looks like a broken image, and
it is a bind address.

### Health checks in the image

Both images declare a `HEALTHCHECK`, and it is the binary itself rather than `curl`:

```dockerfile
HEALTHCHECK CMD ["/usr/local/bin/volucer", "health"]
```

That is not a stylistic choice. The `musl` target is `FROM scratch`, which has no shell, no
`curl` and no `/bin/sh` — so the conventional `HEALTHCHECK CMD curl -f http://localhost/health`
would be a line that never executes. Docker does not report that as a broken check; it reports
the container as `unhealthy` after the start period with no output at all, which is the least
actionable failure a container can have. The subcommand reproduces `curl -f`'s contract — exit
`0` or nonzero — and adds the detail that a shell one-liner throws away.

`volucer health` reads the address from `[status]`, or from `[metrics]` when that is the only
one present, because the two share one listener. It exits `0` only for a `2xx`; anything else,
including a `404` from an image whose configuration lost its `[status]` table, fails the probe.
The reason goes to stderr, which Docker keeps in the health log — so `docker inspect` answers
"why is this unhealthy" with the server's own words rather than a bare timeout:

```console
$ docker inspect --format '{{json .State.Health}}' volucer | jq -r '.Log[].Output'
volucer: could not connect to 127.0.0.1:9080: connection refused
```

Three things decide what it probes, in increasing precedence:

| Setting | Effect |
|---|---|
| `VOLUCER_HEALTH_CONFIG` | The configuration file the probe reads. Defaults to `/etc/volucer/volucer.toml` in the image, so a deployment that overrides `CMD` to mount its file elsewhere overrides this too. |
| `VOLUCER_HEALTH_URL` | A URL to probe instead of reading the configuration. |
| `--url` / `--config` | The flags, which win over both — so a variable someone forgot does not fight a probe typed by hand. |

`/health` is what the image's check uses, and `/ready` is deliberately *not*: Docker restarts
a container whose health check fails, and a server still booting its worker pool must not be
killed for it. `/ready` is the traffic question, and answering it here would turn a slow start
into a crash loop — the exact inversion the two endpoints exist to prevent. Probe it separately
where a load balancer reads it:

```console
docker exec volucer volucer health --ready
```

### Compose

Use the published image, or `build:` if you are working from a checkout:

```yaml
services:
  app:
    image: ghcr.io/sailantis/volucer:0.2.0      # or: build: {context: ., target: musl}
    ports:
      - "8080:8080"
      - "9080:9080"
    volumes:
      - ./volucer.toml:/etc/volucer/volucer.toml:ro
      - ./public:/srv/public:ro
    environment:
      APP_ENV: production
    env_file:
      - secrets.env
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "/usr/local/bin/volucer", "health", "--ready"]
      interval: 10s
      timeout: 3s
      retries: 3
      start_period: 20s
```

The `healthcheck:` block is not redundant with the one in the image: it replaces it, and this
is where `/ready` belongs. `restart: unless-stopped` with an `unhealthy` container is a restart
loop, which is why the image's own default asks the liveness question and this one — read by
whatever routes traffic — asks the readiness one.

`start_period` is the number worth thinking about rather than copying: it has to exceed the
longest time the pool takes to boot, or a server that is starting correctly is restarted for
being slow. `volucer check` reports what the deployment will start before it starts it.

Environment variables reach the configuration through `${...}` — `http = "0.0.0.0:${PORT:-8080}"`
— because volucer deliberately loads no `.env` file of its own. `env_file:` and
`docker run --env-file` already do that, and a second loader would be a second precedence rule
for one job.

### Publishing

The release workflow publishes both images to `ghcr.io/<repository>` on every `v*` tag, for
`linux/amd64` and `linux/arm64`, as a multi-architecture manifest list. It is the last job in
the pipeline and it depends on `publish`, so an image tag never exists for a release whose
archives failed to install.

Nothing is stored as a secret: the workflow authenticates to ghcr with its own `GITHUB_TOKEN`
and the `packages: write` permission, because the package belongs to the repository that built
it.

**The first push creates the package as private, even for a public repository.** GHCR does not
inherit repository visibility — access *rules* inherit, visibility does not — so one manual step
is needed after the first release: the package's **Settings → Danger Zone → Change visibility**,
or the API for an organization-owned package:

```console
gh api --method PATCH /orgs/sailantis/packages/container/volucer -f visibility=public
```

Until that is done, `docker pull` needs a login and the README's one-liner does not work. The
build passes `org.opencontainers.image.source`, which is what links the package to this
repository; an unlinked package cannot be managed from the repository's settings page at all.

Two properties are deliberate and worth not undoing:

- **A build never creates a tag.** Each `(image, architecture)` pair is pushed by digest — an
  addressable but unreferenced `sha256:…` — and the tags are created in one separate job once
  every architecture has been built and smoke-tested. A tag written during the build would be
  pullable while its sibling architecture was still compiling.
- **`glibc` always carries a suffix.** `latest` and `latest-glibc` are different images, so a bare
  pull cannot resolve to the base image by accident.

`.github/workflows/ci.yml` builds both images on every push to `main` and exercises
`packaging/docker/smoke-test.sh` against them, which is the same script the release runs against
the pushed digest. Without that job the first execution of the Dockerfile would be release day.

## The access log and logrotate

`SIGHUP` closes and reopens the log file, which is what makes `logrotate` work:

```text
/var/log/volucer/access.log {
    daily
    rotate 14
    compress
    postrotate
        kill -HUP "$(pidof volucer)"    # or: systemctl reload volucer
    endscript
}
```

The signal reopens unconditionally, before the configuration is even read — a rotation is a
change to the filesystem, and a config that fails to parse is not a reason to keep writing
into a renamed file.

See [Observability](06-OBSERVABILITY.md#the-access-log) for the format.

## Installing the binary

The install scripts are the supported path. See
[Getting Started](00-GETTING-STARTED.md#install).

Each release publishes a per-target archive with a `.sha256` beside it:

```
volucer-0.1.0-x86_64-unknown-linux-gnu.tar.gz
volucer-0.1.0-x86_64-unknown-linux-musl.tar.gz
volucer-0.1.0-aarch64-unknown-linux-gnu.tar.gz
volucer-0.1.0-aarch64-apple-darwin.tar.gz
volucer-0.1.0-x86_64-apple-darwin.tar.gz
volucer-0.1.0-x86_64-pc-windows-msvc.zip
```

Each archive holds the binary, the README, the licence and the systemd unit.

## Running as a different user

Volucer needs to read its document root and write to its ACME cache. It does not need to
write to its own binary, its configuration or the document root, which is what the
`ProtectSystem=strict` and `ReadWritePaths` lines in the unit are for.

If you run it as a non-root user, that user needs read access to the document root and write
access to the ACME cache directory. The `php-cgi` and worker processes inherit the server's
user, so they need the same.

## Upgrading

1. Read the [changelog](../CHANGELOG.md) for the version you are moving to.
2. Install the new binary alongside the old one, or replace it.
3. Run `volucer check --config volucer.toml` with the new binary. It reports what the new
   build resolves, and refuses a configuration the new build will not accept.
4. Restart the service.

A configuration that the new build refuses is refused **at startup**, not silently
reinterpreted — the configuration model is strict, so a removed or renamed key stops the
server rather than being ignored.
