# Operations

This guide covers configuration reloads, reverse-proxy deployments, systemd, containers,
and release operations.

## Reloading

On Unix, send `SIGHUP` to reload and apply the configuration file. Windows has no
`SIGHUP` equivalent, so restart the server there.

```console
kill -HUP $(pidof volucer)      # or: systemctl reload volucer
```

Reloadable configuration is replaced as a single snapshot. New requests use the updated
configuration; in-flight requests continue with the snapshot they started with. HTTP and
FastCGI front ends share the same site configuration, so a reload applies to both.

`SIGHUP` reloads configuration. `SIGTERM` stops the server.

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
- `[status] allow_worker_reset`, in both directions

The following settings require a restart. Volucer reports them during reload:

| Setting | Why it cannot be reloaded |
|---|---|
| `http`, `https`, `fastcgi` | A bound socket cannot move |
| Backends, their commands and worker counts | Those are running processes with live pipes |
| `[server.env]` and a backend's `env` | Existing processes retain their environment; replacement workers could otherwise receive different values from running workers |
| `[server.tls]` `min_version` and `alpn` | They are baked into the `rustls` server config the acceptor holds |
| `[server.proxy]` | Whose forwarded headers are believed is a statement about this deployment's topology |
| `[server.access_log]` path, format, `request_id_header` | Chosen at startup. **The log file is reopened on every `SIGHUP` regardless** |
| `[status]` address | The listener is bound |

### A reload is whole-or-nothing

If configuration parsing or validation fails, Volucer logs the reason and continues serving
with the previous configuration. It does not apply a partially valid configuration.

Files named by `grpc_proto` are read as part of configuration loading. An unreadable file or
a set of files that resolves to no services also causes the reload to fail.

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

`ExecReload` sends `SIGHUP` to reload configuration. If the new configuration is invalid,
the server continues using the previous one.

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

On `SIGTERM`, Volucer stops accepting connections and lets in-flight requests finish. Worker
processes are stopped with the server.

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

```console
volucer health --url http://127.0.0.1:2114/health   # is the process running?
volucer health --url http://127.0.0.1:2114/ready    # should traffic be sent here?
```

`curl -sf` can query these endpoints as well. The `volucer health` command is useful in
minimal containers that do not include curl; see [Health checks in the image](#health-checks-in-the-image).

`/health` returns 200 while the process is running, even if its services are not ready. Use
it as a liveness check to decide whether the process should be restarted.

`/ready` returns the configured `unavailable_status_code` (503 by default) until all
services report ready. Use it to determine whether the instance should receive traffic.

Both endpoints return 404 for other paths. Bind the status listener to loopback or a private
interface and restrict access through the deployment network.

`allow_worker_reset` adds worker-control routes to the same listener. See
[Observability](06-OBSERVABILITY.md#the-worker-control-routes) before turning it on — there
is no authentication for those routes.

## Docker

Use a published image or build one from a source checkout.

### Pulling a published image

Each release publishes musl and glibc images to GitHub Container Registry for
`linux/amd64` and `linux/arm64`:

```console
docker pull ghcr.io/sailantis/volucer:latest          # musl, scratch-based: the recommended one
docker pull ghcr.io/sailantis/volucer:latest-glibc    # Debian-based, if you need a libc
```

The default `musl` image uses a `scratch` base and has no libc dependency. The glibc image
uses a Debian base and always has a `-glibc` suffix:

| Tag | Image |
|---|---|
| `0.2.0`, `0.2`, `latest` | musl, `scratch` base |
| `0.2.0-glibc`, `0.2-glibc`, `latest-glibc` | glibc, `debian:bookworm-slim` |

The `latest` tag changes only for final releases. A pre-release such as `v0.3.0-rc.1`
publishes the `0.3.0` and `0.3` tags without moving `latest`. Pin a version such as `0.2.0`
for deployments that must not change when a new release is published.

The package must be made public after the first release publishes it. See
[Publishing](#publishing); once public, pulling the image does not require `docker login`.

### Building it yourself

`packaging/docker/Dockerfile` builds both images from the same crate. On a 4-vCPU machine
with a cold Cargo cache, builds take approximately 3m45s for musl and 1m40s for glibc:

```console
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

```console
docker run --rm \
    -p 8080:8080 -p 9080:9080 \
    -v "$PWD/volucer.toml:/etc/volucer/volucer.toml:ro" \
    -v "$PWD/public:/srv/public:ro" \
    ghcr.io/sailantis/volucer:latest
```

You can run the image without mounting a configuration file by providing settings on the
command line:

```console
docker run --rm -p 8080:8080 -p 9080:9080 -v "$PWD/public:/srv/public:ro" \
    ghcr.io/sailantis/volucer:latest \
    serve --listen 0.0.0.0:8080 --document-root /srv/public \
        -o server.front_controller=index.php \
        -o backends.app.kind=worker \
        -o "backends.app.command=php /srv/public/worker.php" \
        -o status.address=0.0.0.0:9080
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
address = "0.0.0.0:9080"       # what `docker exec ... health` probes, and what EXPOSE names
```

Listeners bound to `127.0.0.1` are reachable only within the container's network namespace.
Publishing such a port with `-p` does not make the listener externally reachable.

### Health checks in the image

Both images define a `HEALTHCHECK` that runs the Volucer binary directly:

```dockerfile
HEALTHCHECK CMD ["/usr/local/bin/volucer", "health"]
```

The musl image uses `FROM scratch`, which contains neither a shell nor curl. A shell-based
health check would therefore fail to run. The `volucer health` command is self-contained and
returns an exit code suitable for Docker. It reads the listener address from `[status]`, or
from `[metrics]` if that is the only configured listener. It exits successfully only for a
2xx response; other statuses, including 404, fail the check. Diagnostic details are written
to stderr and appear in Docker's health log:

```console
$ docker inspect --format '{{json .State.Health}}' volucer | jq -r '.Log[].Output'
volucer: could not connect to 127.0.0.1:9080: connection refused
```

The command chooses its probe target using these settings, in order of increasing precedence:

| Setting | Effect |
|---|---|
| `VOLUCER_HEALTH_CONFIG` | The configuration file to read. The image defaults to `/etc/volucer/volucer.toml`; set this if the file is mounted elsewhere. |
| `VOLUCER_HEALTH_URL` | A URL to probe instead of reading the configuration. |
| `--url` / `--config` | The flags, which win over both — so a variable someone forgot does not fight a probe typed by hand. |

The image's built-in health check uses `/health`, not `/ready`. A failed Docker health check
may lead to a container restart, so a server should not fail it merely because its worker
pool is still starting. Use `/ready` separately for a load balancer or another component
that decides whether to send traffic:

```console
docker exec volucer volucer health --ready
```

### Compose

Use a published image, or build from the checkout:

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

The Compose `healthcheck:` block replaces the image's default health check. Use `/ready`
there when the check is used to decide whether the application should receive traffic.
Choose a `start_period` long enough for the worker pool to start; otherwise, a slow but
successful startup may fail the health check.

Use environment variables and `${...}` expansion to provide configuration values, for
example `http = "0.0.0.0:${PORT:-8080}"`. Compose's `env_file:` and Docker's
`--env-file` provide the variables; Volucer does not load `.env` files itself.

### Publishing

The release workflow publishes multi-architecture image manifests to
`ghcr.io/<repository>` for `linux/amd64` and `linux/arm64` on each `v*` tag. Image publishing
runs after the archive publishing job succeeds.

The workflow authenticates to GHCR using its `GITHUB_TOKEN` and `packages: write`
permission; no separate secret is required.

The first push creates the package as private, even when the repository is public. After
the first release, change the package visibility to public in its settings or use the
GitHub API for an organization-owned package:

```console
gh api --method PATCH /orgs/sailantis/packages/container/volucer -f visibility=public
```

Until the package is public, pulling it requires authentication. The build sets
`org.opencontainers.image.source` to link the package to the repository.

The workflow has two relevant properties:

- Each image and architecture is pushed by digest. Tags are created only after all
  architectures have built and passed smoke tests, preventing an incomplete multi-platform
  release from being pulled.
- The glibc image always has a `-glibc` suffix; `latest` and `latest-glibc` refer to
  different images.

`.github/workflows/ci.yml` builds and smoke-tests both images on every push to `main`. The
release workflow runs the same smoke test against the published image digests.

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
