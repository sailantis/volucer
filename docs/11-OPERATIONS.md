# Operations

This guide covers configuration reloads, reverse-proxy deployments, systemd, containers,
and release operations.

## Reloading, deploying, and stopping

Three distinct operations, three distinct verbs. They are separated because they mean
different things and a deployment that conflates them either does not deploy or drops requests.

| Verb                    | Signal                | What it does                                                     | Drops a request? |
| ----------------------- | --------------------- | ---------------------------------------------------------------- | ---------------- |
| Reload configuration    | `SIGHUP`              | Re-reads the file and applies the reloadable settings            | No               |
| Roll application code   | `SIGUSR2`             | Replaces the worker processes, keeping every address             | No               |
| Stop                    | `SIGTERM` / `SIGINT`  | Stops accepting, finishes in-flight requests, then exits         | No               |

`SIGUSR1` is deliberately ignored with a warning. It means "reopen the log" in nginx, so a
`logrotate` script written for nginx sends it; volucer reopens its access log on `SIGHUP` instead.
Ignoring it explicitly is not cosmetic — an unregistered signal keeps its default action, which for
`SIGUSR1` is to terminate the process.

**`SIGHUP` does not deploy application code.** It swaps configuration tables — sites,
certificates, the rate limit, gRPC grants, which worker-control routes exist — and it leaves
every PHP process exactly as it was. A `worker` or `spawn_cgi` slot keeps the code it loaded
when it booted, which is the whole point of a persistent worker. Deploying new PHP means
replacing those processes, and that is `SIGUSR2`.

### Reloading configuration

On Unix, send `SIGHUP`. Windows has no `SIGHUP` equivalent, so restart the server there.

```sh
kill -HUP $(pidof volucer)      # or: systemctl reload volucer
```

Reloadable configuration is replaced as a single snapshot. New requests use the updated
configuration; in-flight requests continue with the snapshot they started with. HTTP and
FastCGI front ends share the same site configuration, so a reload applies to both.

### Rolling application code

`SIGUSR2` replaces the application processes with whatever is now on disk, **without
closing a listener and without dropping a request**:

```sh
kill -USR2 $(pidof volucer)
```

It sweeps every slot and replaces the ones that are **not serving a request**. A slot that is
mid-request is left alone and reported, so:

> **A sweep may not finish the roll in one pass.** Each sweep replaces the idle slots; the
> busy ones are left for the next one. For a deployment under continuous load, run the sweep
> again until it reports that nothing was skipped — or accept that the roll completes on the
> next idle moment, which for a busy slot means the next deploy.

Three triggers, one implementation. Every one of them runs the same sweep, so they cannot
disagree about what an idle slot is:

```sh
# 1. The signal, for a systemd unit or a supervisor.
kill -USR2 $(pidof volucer)

# 2. The command, which calls the [status] route. The deployment must grant it:
#      [status.workers] allow = ["workers.restart"]
volucer workers restart --config /etc/volucer/volucer.toml

# 3. The route directly, for a CI step or a supervisor that cannot send signals.
curl -f -XPOST http://127.0.0.1:2114/workers/restart
```

`volucer workers restart` exits **non-zero when the sweep was accepted but is incomplete**, so
a pipeline can loop on it; `--json` prints the per-backend report, which names the slots that
were replaced, skipped as busy, or failed.

What each backend kind does:

| Backend               | A sweep…                                                                     |
| --------------------- | ---------------------------------------------------------------------------- |
| `worker`              | replaces each idle slot's process, in place                                  |
| `spawn_cgi`           | replaces each idle slot's process **on its own address**                      |
| `attach`              | does nothing — the supervisor that started those processes owns them          |
| `external` (php-fpm)  | does nothing — reload php-fpm instead (`SIGHUP`/`USR2` to its own master)     |
| `upstream`            | restarts the origin **if** volucer supervises it; in-flight requests are lost |

The `attach` and `external` rows are reported as *unmanaged*, with the reason, rather than
counted as success. An `upstream` origin is the one kind that cannot be rolled without losing
requests: it serves many at once and has no idle state, so the restart is announced and the
origin's readiness gate is waited out before the sweep reports success.

### Stopping

On `SIGTERM` (or Ctrl-C), Volucer stops accepting connections immediately, then lets the
requests that are already in flight finish, up to `[server] shutdown_timeout_ms` (default
30 s). Requests still running when that expires are abandoned and the count is logged at
`warn`, so a truncated request can be traced to a deadline rather than to the application.

Give your supervisor a longer grace period than that deadline, or it will `SIGKILL` mid-drain
and produce exactly the truncation the drain exists to avoid:

| Supervisor | Setting to check                                             |
| ---------- | ------------------------------------------------------------ |
| systemd    | `TimeoutStopSec` and `KillSignal=SIGTERM`                     |
| Docker     | `docker stop -t <seconds>`, or `stop_grace_period` in Compose |
| Kubernetes | `terminationGracePeriodSeconds`                               |

### What a reload changes

The following settings can be reloaded:

- `[server.tls]` and `[sites.*.tls]` certificates and keys
- `[sites.*]` `hosts`, `document_root`, `front_controller`, `script_extensions`,
  `serve_only`, `default_backend` and `routes`
- `[server.static_files]`, including turning the layer on or off
- `redirect_to_https`
- A backend's `grpc_services` and `grpc_serve_any` — a widened grant admits the next call,
  on the same connection
- A backend's `grpc_proto`. Volucer rereads the `.proto` files and applies newly declared
  services to subsequent calls. Files are not watched; changes take effect after a reload.
- `[server.rate_limit]`, with every tracked caller's bucket carried across when the `key` and
  `header` are unchanged
- `[status.workers] allow`, in both directions

The following settings require a restart. Volucer reports them during reload:

| Setting                                                 | Why it cannot be reloaded                                                                                                      |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `http`, `https`, `fastcgi`                              | A bound socket cannot move                                                                                                     |
| Backends, their commands and worker counts              | Those are running processes with live pipes                                                                                    |
| `[server.env]` and a backend's `env`                    | Existing processes retain their environment; replacement workers could otherwise receive different values from running workers |
| `[server.tls]` `min_version` and `alpn`                 | They are baked into the `rustls` server config the acceptor holds                                                              |
| `[server.proxy]`                                        | Whose forwarded headers are believed is a statement about this deployment's topology                                           |
| `[server.access_log]` path, format, `request_id_header` | Chosen at startup. **The log file is reopened on every `SIGHUP` regardless**                                                   || `[status]` `listen`                                     | The listener is bound                                                                                                          |

### A reload is whole-or-nothing

If configuration parsing or validation fails, Volucer logs the reason and continues serving
with the previous configuration. It does not apply a partially valid configuration.

Files named by `grpc_proto` are read as part of configuration loading. An unreadable file or
a set of files that resolves to no services also causes the reload to fail.

Files named by `include` are reread on every reload, so
a drop-in edited in place takes effect on the next `SIGHUP` without touching the main
configuration file. A missing file, an unreadable directory, or a cycle fails the reload and
leaves the previous configuration serving — the same whole-or-nothing rule.

The site table and certificate table are applied independently, with sites applied first.
If a certificate cannot be read, valid site changes can still take effect.

When `[server.acme]` is configured, issued certificates are retained across reloads. The
certificate table is rebuilt from configuration, then certificates issued by ACME are
registered again. This allows newly configured PEM files to load while keeping issued
certificates active. If a name is no longer configured, its existing certificate remains
registered. To migrate away from ACME, remove `[server.acme]` and restart the server.

## Behind nginx

Volucer can serve HTTP and static files without nginx. If you use nginx, connect it to
Volucer's FastCGI listener:

```toml
[server]
fastcgi = "127.0.0.1:9080"
document_root = "/srv/app/public"
```

A unix socket works in the same place and saves a loopback round trip — `fastcgi_pass` and
`proxy_pass` both take one:

```toml
[server]
fastcgi = "unix:/run/volucer/fcgi.sock"
document_root = "/srv/app/public"
```

Two things differ on a socket, and both are properties of the transport rather than settings: the
socket has no client address, so `REMOTE_ADDR` on the **HTTP** front-end is absent (on the FastCGI
front-end the parameter is nginx's to set and is relayed verbatim), and it has no port, so
`redirect_to_https` is refused for a socket-only listener. See
[Configuration](01-CONFIGURATION.md#unix-socket-listeners). Both front-ends remove a stale socket
path before binding and remove it again on a clean stop, so an unclean shutdown does not need
manual cleanup.

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

In nginx, `try_files` treats every argument except the last as a file test. This means that
the following front-controller configuration is unsafe:

The obvious spelling of a front controller is:

```nginx
location / { try_files /index.php =404; }
location = /index.php { fastcgi_pass ...; }
```

In this configuration, `/index.php` is tested as a file and may be served as static content,
exposing the PHP source. The last `try_files` argument is the fallback, not the first. The
example above instead uses the script as the fallback and routes it to the FastCGI location.

To have Volucer use the original client address for access logs and rate limiting, configure
nginx's trusted address range and forwarded header:

```toml
[server.proxy]
trusted_subnets = ["127.0.0.1"]
trusted_headers = ["X-Forwarded-For"]
```

## HTTP/3: UDP reachability and MTU

HTTP/3 is served on the UDP address in `[server.http3] listen`, and browsers only learn about
it from the `Alt-Svc` header Volucer sends on TLS responses. Three things therefore have to be
true at once, and each fails in a different way.

**The UDP port must be reachable.** This is the common one, and it fails silently: the TCP
listener answers normally, `Alt-Svc` advertises the QUIC endpoint, the browser attempts QUIC,
its datagrams are dropped, and it falls back to TCP after a retry delay. Nothing errors, and
the only symptom is a site that feels slow on the first request.

```bash
# Check the container or host publishes UDP, not only TCP:
docker run -p 443:443/tcp -p 443:443/udp ...

# Check the firewall:
sudo ufw allow 443/tcp
sudo ufw allow 443/udp        # the line that is easy to forget

# From outside, with a client that speaks HTTP/3:
curl --http3-only -sI https://example.com
```

If `curl` needs `--http2` to succeed, that is the signature of exactly this problem.

**The path MTU must not black-hole large datagrams.** QUIC sends most of its traffic in
datagrams near the path MTU, and a link that drops ICMP "fragmentation needed" will carry the
handshake and then stall. Volucer's `max_body_bytes` is not the relevant limit — the failure
is in the network. `quinn` does path MTU discovery, so a modern path recovers; a path behind a
tunnel or a middlebox that filters ICMP does not. Symptoms are HTTP/3 connections that
complete their handshake and then hang on the first large response.

**`Alt-Svc` has a lifetime.** `alt_svc_ma_seconds` (default one day) is how long a browser
caches the endpoint. Removing `[server.http3]` from a running deployment therefore does not
take effect immediately for returning visitors: they will keep attempting the UDP endpoint
until their cached value expires, and fall back. That is the reason not to lower the default
carelessly, and the reason a rollback of HTTP/3 is a one-day event rather than an instant one.

`volucer check` prints the `[server.http3]` block, and the listener's shutdown line reports
requests, connections, handshake failures and oversized header sections — a non-zero
`handshake failure` count on a port that should be idle is the signal that something between
the client and the server is interfering with UDP.

## `systemd`

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

`ExecReload` sends `SIGHUP` to reload configuration. If the new configuration is invalid,
the server continues using the previous one.

**It does not deploy application code.** To put new PHP in front of the same addresses, send
`SIGUSR2` — there is no `ExecReload=` verb for it because `systemd` maps a unit's reload to one
signal, and the two operations are different:

```sh
# Roll the workers with the code that is now on disk. Keeps both listeners open and
# replaces only the slots that are idle, so no request is dropped.
systemctl kill -s SIGUSR2 volucer
```

A deploy therefore looks like: copy the new code into place, `systemctl kill -s SIGUSR2 volucer`,
then (only if the configuration also changed) `systemctl reload volucer`. Reversing the order
would deploy the old code under the new configuration.

You can also provide configuration through command-line arguments instead of a
configuration file:

```ini
ExecStart=/usr/local/bin/volucer serve \
    --listen 127.0.0.1:8080 \
    -o server.document_root=/srv/public \
    -o server.front_controller=index.php \
    -o backends.app.kind=worker \
    -o "backends.app.command=php /srv/public/worker.php" \
    -o backends.app.workers=0
```

On `SIGHUP`, Volucer reapplies the same command-line overrides. Without a configuration
file, there are no file changes to read. Changes to `ExecStart` take effect only after the
service restarts.

On `SIGTERM`, Volucer stops accepting connections immediately, then lets the requests that are
already running finish before exiting. Worker processes are stopped with the server.

That grace period has a budget: [`server.shutdown_timeout_ms`](01-CONFIGURATION.md#server), thirty
seconds by default. When it expires, whatever is still in flight is abandoned and the count is
logged at `warn`, so a truncated response is visible rather than indistinguishable from a fast one.

**The budget must be shorter than whatever waits before killing the process.** `systemd`'s
`TimeoutStopSec` and a container's `stop_grace_period` both default to thirty seconds, which is why
the default budget matches them — an operator who has changed neither gets a drain that completes.
If you raise `shutdown_timeout_ms`, raise the supervisor's timeout above it as well, or `SIGKILL`
will land mid-drain and truncate exactly what the setting exists to protect.

```toml
[server]
shutdown_timeout_ms = 10000     # default 30000; 0 means "do not wait at all"
```

Store the ACME cache under the systemd state directory and configure the cache path to match
`StateDirectory`:

```toml
[server.acme]
cache = "/var/lib/volucer/acme-cache"
```

### Secrets and environment

Volucer does not load a `.env` file itself. Use systemd's `EnvironmentFile=`, Docker's
`--env-file`, or Compose's `env_file:` to provide variables. Volucer can expand those
variables in configuration using `${...}`:

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

Variables set directly in the unit take precedence over values from `EnvironmentFile=`.
Expanded values can be passed to applications through `[server.env]` or a backend's `env`
table. `config.describe` displays variable names without their values.

`volucer check` performs the same expansion as `serve` and reports missing variables before
you restart the service.

## Health checks

The `[status]` listener provides separate liveness and readiness probes:

```toml
[status]
address = "127.0.0.1:2114"
```

```sh
volucer health --url http://127.0.0.1:2114/health   # is the process running?
volucer health --url http://127.0.0.1:2114/ready    # should traffic be sent here?
```

`curl -sf` can query these endpoints as well. The `volucer health` command is useful in
minimal containers that do not include curl; see [Health checks in the image](#health-checks-in-the-image).

`/health` returns 200 while the process is running, even if its services are not ready. Use
it as a liveness check to decide whether the process should be restarted.

`/ready` returns the configured `unavailable_status` (503 by default) until all
services report ready. Use it to determine whether the instance should receive traffic.

Both endpoints return 404 for other paths. Bind the status listener to loopback or a private
interface and restrict access through the deployment network.

`[status.workers] allow` adds worker-control routes to the same listener. See
[Observability](06-OBSERVABILITY.md#the-worker-control-routes) before turning it on — there
is no authentication for those routes, and naming `workers.list` alone grants only the read.

## Docker

Use a published image or build one from a source checkout.

### Pulling a published image

Each release publishes musl and glibc images to GitHub Container Registry for
`linux/amd64` and `linux/arm64`:

```sh
docker pull ghcr.io/sailantis/volucer:latest          # musl, scratch-based: the recommended one
docker pull ghcr.io/sailantis/volucer:latest-glibc    # Debian-based, if you need a libc
```

The default `musl` image uses a `scratch` base and has no libc dependency. The glibc image
uses a Debian base and always has a `-glibc` suffix:

| Tag                                        | Image                         |
| ------------------------------------------ | ----------------------------- |
| `0.3.0`, `0.3`, `latest`                   | musl, `scratch` base          |
| `0.3.0-glibc`, `0.3-glibc`, `latest-glibc` | glibc, `debian:bookworm-slim` |

The `latest` tag changes only for final releases. A pre-release such as `v0.4.0-rc.1`
publishes the `0.4.0` and `0.4` tags without moving `latest`. Pin a version such as `0.3.0`
for deployments that must not change when a new release is published.

The package must be made public after the first release publishes it. See
[Publishing](#publishing); once public, pulling the image does not require `docker login`.

### Building it yourself

`packaging/docker/Dockerfile` builds both images from the same crate. On a 4-vCPU machine
with a cold Cargo cache, builds take approximately 3m45s for musl and 1m40s for glibc:

```sh
docker build -f packaging/docker/Dockerfile --target musl -t volucer .           # the host's architecture
docker build -f packaging/docker/Dockerfile --target glibc -t volucer:glibc .
```

The `-f` option is required because the Dockerfile is in `packaging/docker/`. Use the
repository root as the build context because `COPY . .` needs the full Cargo workspace.

`--target musl` selects the Rust target based on `--platform`. For example,
`docker build -f packaging/docker/Dockerfile --platform linux/arm64 --target musl` produces
an `aarch64` binary. If an architecture has no musl target, the build fails rather than
falling back to another architecture.

The two images are 23 MB (`musl`, `scratch`) and 149 MB (`glibc`, `debian:bookworm-slim`).

The binary is built into the image, so there is no archive to download or verify separately.
`.dockerignore` excludes `target/` and `.git/` from the build context. Without this, the
repository's potentially large `target/` directory would be uploaded and copied into a layer
before the build begins.

### Running it

Both images use the same runtime contract. Replace the local image name with a published
image to use a release build:

```sh
docker run --rm \
    -p 8080:8080 -p 9080:9080 \
    -v "$PWD/volucer.toml:/etc/volucer/volucer.toml:ro" \
    -v "$PWD/public:/srv/public:ro" \
    ghcr.io/sailantis/volucer:latest
```

You can run the image without mounting a configuration file by providing settings on the
command line:

```sh
docker run --rm -p 8080:8080 -p 9080:9080 -v "$PWD/public:/srv/public:ro" \
    ghcr.io/sailantis/volucer:latest \
    serve --listen 0.0.0.0:8080 --document-root /srv/public \
        -o server.front_controller=index.php \
        -o backends.app.kind=worker \
        -o "backends.app.command=php /srv/public/worker.php" \
        -o status.listen=0.0.0.0:9080
```

The image's `ENTRYPOINT` is the Volucer binary. Arguments after the image name replace the
default `serve --config /etc/volucer/volucer.toml`, so include the `serve` subcommand.

The image's `HEALTHCHECK` normally reads `[status]` from a configuration file. For a
fileless container, configure the check with a URL such as
`HEALTHCHECK CMD ["/volucer", "health", "--url", "http://127.0.0.1:9080/health"]`. See
[Health checks in the image](#health-checks-in-the-image).

In a container, configure both listeners to bind to addresses reachable from outside the
container:

```toml
[server]
http = "0.0.0.0:8080"          # not 127.0.0.1: the port has to be reachable from outside
document_root = "/srv/public"

[status]
listen = "0.0.0.0:9080"        # what `docker exec ... health` probes, and what EXPOSE names
```

Listeners bound to `127.0.0.1` are reachable only within the container's network namespace.
Publishing such a port with `-p` does not make the listener externally reachable.

### Adding PHP to the image

Both images contain only the binary. The `musl` image is `scratch`, with no libc, so PHP cannot run in it. The `glibc` image is Debian-based and has no PHP runtime. A `worker` with `command = "php worker.php"`, as in the [README](../README.md), therefore needs an image that includes PHP. Build one from the official `php` image and copy the binary from the `-glibc` image:

```dockerfile
# Use `-cli`, not `-fpm` or `-apache`. Those two are built with `--disable-cgi`, so a
# `spawn_cgi` pool would have no `php-cgi` to start. `-cli` builds both SAPIs, so this one base
# supports a `worker` (`php worker.php`) and a `spawn_cgi` pool (`php-cgi -b …`).
# The Debian suite is named explicitly. The plain `8.5-cli` tag follows Debian's default, which
# currently resolves to `-trixie`. Naming the suite avoids a surprise when that default changes.
FROM php:8.5-cli-bookworm

# Use the `-glibc` image, not the default `musl` tag. This base is Debian, and the `glibc` stage
# of `packaging/docker/Dockerfile` builds the matching binary. Pin the version.
COPY --from=ghcr.io/sailantis/volucer:0.3.0-glibc /usr/local/bin/volucer /usr/local/bin/volucer

# Add the extensions the application needs. Each one is a compile step in this Dockerfile,
# not a version in this project's release matrix.
RUN docker-php-ext-install pdo_mysql

# Composer is not in the base image. Copying its binary avoids a separate installer step.
COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

WORKDIR /srv/app

# Copy the application. Exclude `vendor/` and local environment files in `.dockerignore`, so
# dependencies are installed inside the image.
COPY . /srv/app
RUN composer install --no-dev --no-interaction --optimize-autoloader

# Match the published image's contract, so an argument after the image name is passed to Volucer.
# This replaces the base image's `docker-php-entrypoint`, which would treat `docker run … version`
# as a program named `version`. `php` remains available through `docker exec` and a backend's
# `command`.
EXPOSE 8080 9080

# The base image defines no health check, and the binary serves as its own probe. Use `/health`
# rather than `/ready`, as the published images do: a slow worker pool must not fail the check
# and restart the container. The variable tells the probe where the file is, because the working
# directory belongs to the application, not the configuration.
ENV VOLUCER_HEALTH_CONFIG=/etc/volucer/volucer.toml

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/volucer", "health"]

ENTRYPOINT ["/usr/local/bin/volucer"]
CMD ["serve", "--config", "/etc/volucer/volucer.toml"]
```

The configuration then refers to `php` by name. Both binaries are already on `PATH`
(`/usr/local/bin/php` and `/usr/local/bin/php-cgi`), so no path needs to be written into it:

```toml
[server]
http = "0.0.0.0:8080"
document_root = "/srv/app/public"    # the WORKDIR above, plus the application's own public/
front_controller = "index.php"

# Address that `volucer health` probes. Use `0.0.0.0` in both places, so the probe reaches the
# listener inside the container and `-p` reaches it from outside.
[status]
listen = "0.0.0.0:9080"

[backends.app]
kind = "worker"
command = "php worker.php"
working_dir = "/srv/app"
```

Bind both listeners to `0.0.0.0`, as explained in [Running it](#running-it). Run it the same way:

```sh
docker run --rm -p 8080:8080 -p 9080:9080 \
    -v "$PWD/volucer.toml:/etc/volucer/volucer.toml:ro" \
    my-app
```

The document root and application are already in the image, so only the configuration is
mounted. An edit takes effect on `docker restart`, without a rebuild.

Review these details before the first build:

- **PHP 8.5 builds OPcache into the binary.** Do not run `docker-php-ext-install opcache`.
  From 8.5, the extension is always compiled in and loaded, the `--enable-opcache` configure
  flags are removed, and no `opcache.so` is produced for that command to install. A
  `spawn_cgi` pool uses it through `opcache.enable`, which defaults to on. A `worker` runs
  under the CLI SAPI, where `opcache.enable_cli` defaults to off. Leaving it off has no cost,
  because a resident worker compiles its code once at boot. On PHP 8.4 and older, the
  extension must still be installed.
- **Extensions that link a system library need that library's `-dev` package first.** For
  example, `intl` requires `libicu-dev`, and `gd` requires `libpng-dev libjpeg62-turbo-dev
libfreetype6-dev`. Install these with `apt-get install` in a layer before
  `docker-php-ext-install`. The image includes `ca-certificates`, so ACME issuance and outbound
  TLS work without changes. Place settings in `$PHP_INI_DIR/conf.d/`
  (`/usr/local/etc/php/conf.d`), starting from the `php.ini-production` file the image ships.
- **A copied binary does not bring a user with it.** This image runs as `root` unless
  configured otherwise. The published `glibc` image, by contrast, creates a `volucer` user. To
  drop privileges, add `USER www-data` after the `RUN composer install` line, and `chown` the
  application directory to that user. In an Azera application, `writable/` must remain
  writable by the user the workers run as.

The base image defines no `HEALTHCHECK`, so the recipe adds one, using the line from
[Health checks in the image](#health-checks-in-the-image). It works here for the same reason it
works there: the check runs the binary, not `curl`.

The recipe also replaces the base image's `ENTRYPOINT`, so an argument after the image name is
passed to Volucer. To run PHP in that container, name the program explicitly:

```sh
docker run --rm --entrypoint php my-app -m
docker exec my-app php -m                 # in a running container
```

### Health checks in the image

Both images define a `HEALTHCHECK` that runs the Volucer binary directly:

```dockerfile
HEALTHCHECK CMD ["/usr/local/bin/volucer", "health"]
```

The musl image uses `FROM scratch`, which contains neither a shell nor curl. A shell-based
health check would therefore fail to run. The `volucer health` command is self-contained and
returns an exit code suitable for Docker. It reads the listener address from `[status]`. It
exits successfully only for a 2xx response; other statuses, including 404, fail the check.
Diagnostic details are written to stderr and appear in Docker's health log:

```sh
$ docker inspect --format '{{json .State.Health}}' volucer | jq -r '.Log[].Output'
volucer: could not connect to 127.0.0.1:9080: connection refused
```

The command chooses its probe target using these settings, in order of increasing precedence:

| Setting                 | Effect                                                                                                                        |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `VOLUCER_HEALTH_CONFIG` | The configuration file to read. The image defaults to `/etc/volucer/volucer.toml`; set this if the file is mounted elsewhere. |
| `VOLUCER_HEALTH_URL`    | A URL to probe instead of reading the configuration.                                                                          |
| `--url` / `--config`    | The flags, which win over both — so a variable someone forgot does not fight a probe typed by hand.                           |

The image's built-in health check uses `/health`, not `/ready`. A failed Docker health check
may lead to a container restart, so a server should not fail it merely because its worker
pool is still starting. Use `/ready` separately for a load balancer or another component
that decides whether to send traffic:

```sh
docker exec volucer volucer health --ready
```

### Compose

Use a published image, or build from the checkout:

```yaml
services:
  app:
    image: ghcr.io/sailantis/volucer:0.3.0 # or: build: {context: ., target: musl}
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

The Compose `healthcheck:` block replaces the image's default health check. Use `/ready`
there when the check is used to decide whether the application should receive traffic.
Choose a `start_period` long enough for the worker pool to start; otherwise, a slow but
successful startup may fail the health check.

Use environment variables and `${...}` expansion to provide configuration values, for
example `http = "0.0.0.0:${PORT:-8080}"`. Compose's `env_file:` and Docker's
`--env-file` provide the variables; Volucer does not load `.env` files itself.

## The access log and logrotate

On `SIGHUP`, Volucer closes and reopens the access-log file. Configure `logrotate` to signal
the server after rotating the file:

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

The log is reopened even if configuration reload fails. For supported formats and details,
see [Observability](06-OBSERVABILITY.md#the-access-log).

## Installing the binary

Use the install scripts to install the binary. See
[Getting Started](00-GETTING-STARTED.md#install).

Each release includes a target-specific archive and a corresponding `.sha256` checksum file:

```
volucer-0.1.0-x86_64-unknown-linux-gnu.tar.gz
volucer-0.1.0-x86_64-unknown-linux-musl.tar.gz
volucer-0.1.0-aarch64-unknown-linux-gnu.tar.gz
volucer-0.1.0-aarch64-apple-darwin.tar.gz
volucer-0.1.0-x86_64-apple-darwin.tar.gz
volucer-0.1.0-x86_64-pc-windows-msvc.zip
```

Each archive contains the binary, README, license, and systemd unit.

## Running as a different user

Volucer needs read access to the document root and write access to the ACME cache. It does
not need write access to its binary, configuration, or document root. The systemd unit's
`ProtectSystem=strict` and `ReadWritePaths` settings reflect these requirements.

When running Volucer as a non-root user, grant that user read access to the document root
and write access to the ACME cache. PHP-CGI and worker processes run as the same user and
need the corresponding access.

## Upgrading

1. Review the [changelog](../CHANGELOG.md) for the target version.
2. Install the new binary alongside the current one, or replace the current binary.
3. Run `volucer check --config volucer.toml` with the new binary to verify the configuration.
4. Restart the service.

Configuration is strict. If the new version no longer accepts a setting, `volucer check`
reports the problem and the server refuses to start rather than silently ignoring or
reinterpreting the setting.
