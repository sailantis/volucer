# Getting Started

Install Volucer, connect it to an application, and serve requests over HTTP or FastCGI.

## What Volucer does

Volucer is a cross-platform application server distributed as one binary. It does not
embed a language runtime: it starts and supervises application processes, connects to
existing backends, and serves applications over HTTP or FastCGI.

Choose a backend for each route:

| Backend           | How it works                                                                                  | Use it when                                                                                        |
| ----------------- | --------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| **Worker**        | The app starts once and stays resident. The app resets request-scoped state between requests. | You use a Volucer worker SDK or implement the worker protocol, and want to avoid repeated startup. |
| **FastCGI**       | A FastCGI backend handles requests and resets request state.                                  | You want to keep an existing FastCGI application and its request lifecycle.                        |
| **HTTP upstream** | Volucer routes requests to an HTTP server.                                                    | Your application already runs its own HTTP server.                                                 |

All routes use the same listener. For example, one Volucer process can serve `/api` with
a worker and `/admin` through FastCGI.

**FastCGI is a protocol, not a PHP feature.** Volucer can attach to an existing FastCGI
pool with the `external` backend, or start and supervise a compatible server with
`spawn_cgi`. This includes `php-fpm`, `php-cgi`, and FastCGI applications built with
WSGI, Rack, or Go.

Volucer runs on Linux, macOS, and Windows. Windows does not provide `php-fpm`; use
`php-cgi` there for a FastCGI-based PHP deployment. Worker SDKs are available for PHP,
Python, and Ruby.

## Requirements

- An application that uses one of Volucer's supported backends. Any framework can work;
  the backend determines how Volucer communicates with it.
- **PHP 8.2 or newer** if you plan to use the PHP worker SDK.
- No Rust toolchain or build step is needed to install and run the prebuilt binary.

## Install

### Linux and macOS

```sh
curl -fsSL https://github.com/sailantis/volucer/releases/latest/download/install.sh | sh
```

The installer puts the binary in `~/.local/bin` and adds that directory to `PATH` if
needed. It updates the startup file used by your shell (`~/.bashrc`, `~/.profile`,
`~/.zshrc`, or `~/.config/fish/config.fish`) and prints the exact change it made. Set
`VOLUCER_MODIFY_PATH=0` to install without editing a shell startup file.

### Windows

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/sailantis/volucer/releases/latest/download/install.ps1 | iex"
```

The installer puts the binary in `%LOCALAPPDATA%\volucer\bin` and adds it to your user
`PATH`. Open a new shell for the change to take effect. Pass `-ModifyPath 0` (or set
`VOLUCER_MODIFY_PATH=0`) to leave `PATH` unchanged.

Both installers find the latest release, download the archive for your platform, and
verify its SHA-256 before extracting or installing anything. If a step fails, the
existing binary is left in place.

The SHA-256 file detects corruption in transit, but by itself does not prove who published
the archive if both files come from the same server. Releases also include a provenance
attestation created with `actions/attest-build-provenance`. When possible, the installers
verify it with `gh attestation verify`, which checks the GitHub Actions identity and the
Sigstore transparency log.

Attestation verification requires the GitHub CLI and a GitHub login, so it is best-effort
by default. The installer reports whether verification succeeded. Set
`VOLUCER_VERIFY=require` (or `-Verify require` in PowerShell) to refuse installation if
verification cannot be completed. A failed verification is fatal unless verification is
explicitly disabled with `off`.

The installers read settings from environment variables, which work with both
`curl | sh` and `irm | iex`. They support these settings:

| Variable               | Effect                                                                                    |
| ---------------------- | ----------------------------------------------------------------------------------------- |
| `VOLUCER_VERSION`      | Install a specific tag instead of the newest release.                                     |
| `VOLUCER_INSTALL_DIR`  | Where to put the binary.                                                                  |
| `VOLUCER_FORCE=1`      | Replace an existing binary.                                                               |
| `VOLUCER_MODIFY_PATH`  | `1` (default) or `0` — whether the installer may add the install directory to `PATH`.     |
| `VOLUCER_DOWNLOAD_URL` | A mirror, a GitHub Enterprise host, or an air-gapped host.                                |
| `VOLUCER_API`          | The release API root.                                                                     |
| `VOLUCER_VERIFY`       | `auto` (default), `require` or `off` — whether to check the build provenance attestation. |

`install.ps1` also accepts parameters (`-Version`, `-InstallDir`, `-Force`, `-BaseUrl`,
`-Verify`, and `-ModifyPath`). If a parameter is not supplied, it uses the corresponding
environment variable.

On Linux and macOS, the shell installer also accepts `VOLUCER_LIBC` (`gnu` or `musl`)
and `VOLUCER_ARCH` (`x86_64` or `aarch64`). On Windows, `-ModifyPath` (or
`VOLUCER_MODIFY_PATH`) controls whether the installer edits the user `PATH`; it defaults
to `1`.

### In a container

Every release publishes `linux/amd64` and `linux/arm64` images to GitHub Container Registry:

```sh
docker pull ghcr.io/sailantis/volucer:latest          # musl, scratch-based: the recommended one
docker pull ghcr.io/sailantis/volucer:latest-glibc    # Debian-based, if you need a libc
```

The default image uses musl and is based on `scratch`; the glibc image always carries the
`-glibc` suffix. For reproducible deployments, pin a version tag such as `0.3.0` instead of
`latest`. The package has to be made public once after its first release; see
[Publishing](11-OPERATIONS.md#publishing).

To build either image from a checkout instead, use `packaging/docker/Dockerfile`:

```sh
docker build -f packaging/docker/Dockerfile --target musl -t volucer .
```

The image's Docker `HEALTHCHECK` runs `volucer health`. The command reads its probe
address from `[status]`, so it also works in the `scratch` image, which contains no shell
or `curl`. See [Docker operations](11-OPERATIONS.md#docker) for `docker run`, Compose,
and guidance on when to use `/health` or `/ready`.

### Directly

Download the archive for your platform from the
[releases page](https://github.com/sailantis/volucer/releases), verify it using the
adjacent `.sha256` file, and put the binary on your `PATH`.

## Check your setup

Run `volucer check` before starting the server:

```sh
volucer check
```

The command validates the configuration and reports the effective listeners, backends,
resolved document root and front controller, and any PHP binaries it finds. Use it to
catch configuration problems before serving traffic.

```
configuration: volucer.toml
  listeners:
    http:    0.0.0.0:8080
    https:   (disabled)
    fastcgi: (disabled)
    max body: 33554432 (32 MiB; bounds both front-ends, not an upstream)
    rate limit: none (server.rate_limit is unset; nothing is throttled)
  tls:              (disabled)
  acme:             (disabled)
  document root: /srv/app/public
    exists:           yes
    front controller: index.php
      present:        yes
  backends:
    app: worker (default)
      per-request reset: no (application-scoped worker)
      concurrency:       `workers` connections
      transport:         pipe
  routes:
    (none: every request goes to the default backend)
  sites:
    (none: [server] describes the only site)
  static files:
    all
    enabled for the HTTP listener
      precompressed:   no
      directory index: index.html, with a redirect from the bare path
      dotfiles:        Deny
      cache-control:   public, max-age=3600
  access log: none (server.access_log is unset)
  trace context: W3C traceparent is read, continued, and forwarded to backends
  telemetry: none (telemetry is unset)
  status:           (disabled: no /health, no /ready, and no worker routes)
  PHP binaries:
    cgi  PHP 8.3.33 unknown  /usr/bin/php-cgi
    cli  PHP 8.3.33 NTS      /usr/bin/php
configuration OK
```

If a `warnings:` block appears, review it before deployment. It can identify a missing
path, a front controller that is not under the document root, or a worker command whose
program is not on `PATH`. Warnings do not make `check` fail: `configuration OK` means the
configuration is internally consistent, not necessarily that it matches your intent. A
zero exit code with warnings can still lead to a deployment that starts but behaves
incorrectly.

For `spawn_cgi`, `check` prints the total process count. For a `command` driver it is
`workers × concurrency`; for `php-cgi` it is `workers × (children + 1)`, because each
master also spawns its children. If `workers = 0`, Volucer first resolves it to the
number of logical CPUs. Use the resulting count to plan memory, file descriptors, and a
container's `pids_limit`.

For a `command` containing `{concurrency}`, `check` also prints the resolved command.
This shows the value Volucer will pass to the program: `concurrency = 0` is treated as
one, so the command receives `1`. `{address}` remains unchanged in this output because
the pool assigns a separate address to each slot at runtime.

`volucer check` requires a configuration file. If you do not have one yet, create the
minimal file shown below first.

If Volucer cannot find your PHP installation, provide the directory containing its
binaries:

```sh
volucer check --php-dir /usr/local/php/bin
```

## Write a configuration file

By default, Volucer reads `volucer.toml` from the working directory. This minimal
configuration listens for HTTP requests and forwards them to an existing PHP-FPM socket:

```toml
[server]
http = "127.0.0.1:8080"
document_root = "./public"
default_backend = "app"

[backends.app]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"
```

The configuration has three important parts:

- **`[server]`** configures the listener, document root, and default backend.
- **`[backends.app]`** defines how the application is reached. Here, `external` connects to
  a PHP-FPM process that is already running.
- **`default_backend`** selects the backend for requests that do not match a route. It
  belongs under `[server]` because that table also represents the default, unnamed site.
  A `[sites.<name>]` table can override it for a particular host, along with settings such
  as `document_root`.

This is a complete FastCGI setup: Volucer serves requests to PHP-FPM without requiring
nginx in front. The same `external` backend can connect to any FastCGI server; change the
socket to the one used by that application:

```toml
[backends.app]
kind = "external"
socket = "unix:/run/app/py.sock"
```

To run a persistent worker instead, replace the backend configuration:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
```

The application needs a `worker.php` entry point and the worker SDK. See
[Worker Applications](07-WORKER-APPLICATIONS.md) for implementation details.

## Start the server

```sh
volucer serve
```

With the example configuration above, the application is available at
<http://127.0.0.1:8080>.

`volucer serve` accepts these options:

| Option                                    | Effect                                                                                                                |
| ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `-c`, `--config <PATH>`                   | The configuration file. Defaults to `volucer.toml` in the working directory, and is optional — see below.             |
| `-o`, `--override <KEY=VALUE>`            | Set one key, in `dot.notation`. Repeatable.                                                                           |
| `--listen <ADDR>`                         | Shorthand for `-o server.http=<ADDR>`.                                                                                |
| `--fastcgi <ADDR>`                        | Shorthand for `-o server.fastcgi=<ADDR>`.                                                                             |
| `--document-root <PATH>`, `--root <PATH>` | Override `server.document_root`.                                                                                      |
| `--front-controller <NAME>`               | Override `server.front_controller`. Set it to an empty string to serve only real files.                               |
| `--php-cgi [<PATH>]`                      | Serve a `php-cgi` pool as the default backend — see below.                                                            |
| `--worker <COMMAND>`                      | Serve a resident worker as the default backend, e.g. `--worker "php worker.php"`.                                     |
| `--fcgi <COMMAND>`                        | Serve any FastCGI server as the default backend, e.g. `--fcgi "python fcgi_app.py --bind {address}"`.                 |
| `--workers <N>`                           | How many slots the chosen backend starts. Requires one of the three flags above.                                      |
| `--concurrency <N>`                       | How many requests one `--fcgi` slot serves at once. `--fcgi` only. `--capacity` is the old spelling and still parses. |
| `--max-connections <N>`                   | Maximum concurrent client connections. Defaults to `512`.                                                             |

Command-line options override values in the configuration file. Use them to supply
deployment-specific settings without maintaining a separate file for each environment.

### The short command

For local development with PHP, these options are often enough:

```sh
volucer serve --listen 127.0.0.1:8080 --root public --php-cgi
```

This starts Volucer without a configuration file or `-o` overrides. `--php-cgi` configures
a `spawn_cgi` backend named `cgi`, sets its binary to `php-cgi`, and makes it the default
backend. Volucer does not infer an application backend from the working directory; without
a backend in the file or on the command line, it reports the missing configuration.

With no value, `--php-cgi` looks for `php-cgi` using the same search order as
`volucer check`: `PATH` first, then conventional installation directories. On Windows,
the search also uses `PATHEXT`. Pass a path to select a specific executable:

```sh
volucer serve --listen 127.0.0.1:8080 --root public \
    --php-cgi=C:\php-8.3.31\php-cgi.exe --workers=5
```

`--workers` sets that backend's `workers` value. If omitted, Volucer starts one slot per
logical CPU. `volucer check` accepts the same options, so you can validate a command before
serving traffic. Volucer applies these flags before `-o` overrides, so
`-o backends.cgi.workers=9` takes precedence over `--workers`.

### The other two modes

You can also select a worker or another FastCGI server from the command line.
**Worker mode** starts a persistent process and boots the application once:

```sh
volucer serve --listen 127.0.0.1:8080 --root public --worker "php worker.php" --workers=5
```

This sets `backends.app.kind=worker`, its `command`, and `server.default_backend`. The
application needs a worker SDK; see [Worker Applications](07-WORKER-APPLICATIONS.md).

The document root and the worker command resolve from different directories.
`--root public` sets the document root for static files and front-controller lookup. The worker's
`command` runs from the **working directory** where you started `volucer serve`. In this
example, `php worker.php` refers to the file beside `public/`, not inside it. Keeping the
worker entry point outside the document root is a common layout:

```text
my-project/
├── worker.php          ← --worker "php worker.php"
└── public/             ← --root public
    ├── index.php
    └── style.css
```

For a FastCGI application, such as a WSGI, Rack, or Go server, use `--fcgi`:

```sh
volucer serve --listen 127.0.0.1:8080 --root public \
    --fcgi "python fcgi_app.py --bind {address}" --workers=2 --concurrency=4
```

Include `{address}` in the command. Volucer allocates an address for each slot; if the
program binds its own address instead, Volucer cannot reach it. `check` warns if the
placeholder is missing.

`--workers` and `--concurrency` control different levels of parallelism. `--workers`
sets the number of FastCGI slots Volucer starts (one by default); `--concurrency` sets
how many requests each slot can handle. Volucer runs the command once per slot and
provides a separate `{address}`. For example,
`--fcgi "gunicorn app.wsgi --workers 4 --bind {address}"` needs `--concurrency 4`.
Setting `--workers 2` then starts two such slots. `--concurrency` requires `--fcgi`;
worker mode handles one request at a time, while `php-cgi` uses `children` for the
equivalent setting. The old option name `--capacity` is still accepted.

By default, `--fcgi` serves only real files. A WSGI or Rack application is not an
`index.php` front controller, so the PHP-style default would return 404 for paths that do
not name a file. Pass `--front-controller app.py` if the application has a front
controller.

The three backend options (`--php-cgi`, `--worker`, and `--fcgi`) are mutually exclusive.

### Run without a configuration file

A configuration file and the command line are two ways to provide settings. If you do
not pass `-c` and there is no `volucer.toml` in the working directory, configure the
server with `-o`, `--listen`, and `--fastcgi`. This can be useful for a container command
or systemd unit:

```sh
volucer serve \
    --listen 0.0.0.0:8080 \
    -o server.document_root=public \
    -o server.front_controller=index.php \
    -o backends.app.kind=worker \
    -o backends.app.command="php worker.php" \
    -o backends.app.workers=8
```

```ini
# the same deployment as a unit, with no file to mount
ExecStart=/usr/local/bin/volucer serve --listen 0.0.0.0:8080 \
    -o server.document_root=/srv/public \
    -o backends.app.kind=worker \
    -o backends.app.command="php /srv/public/worker.php"
```

Keep these rules in mind when setting options on the command line:

- **Values use TOML literal syntax.** For example, `-o backends.app.workers=8` sets a
  number. `-o server.http=127.0.0.1:8080` is treated as a string because it is not a valid
  TOML literal; quote the value explicitly with
  `-o 'server.http="0.0.0.0:8080"'` when needed.
- **`--listen` and `--fastcgi` are applied before `-o`.** An explicit `-o server.http=…`
  therefore overrides `--listen …`, regardless of the order in which the options appear.
- **Unknown keys are errors.** A typo such as `-o server.htpp=…` stops startup and names
  the invalid key instead of silently using a default.

Use TOML dotted-key syntax to override values inside tables. For example,
`-o backends.app.workers=8` sets a worker count, and `-o sites.app.document_root=public`
sets a site's document root. If a key contains a dot, quote that key:
`-o 'server.env."APP.NAME"=x'`.

To override fields in an **array of tables**, include the element's index:

```sh
volucer serve --listen 0.0.0.0:8080 \
    -o server.routes[0].path_prefix=/api \
    -o server.routes[0].backend=app \
    -o server.routes[1].path_prefix=/admin
```

An index updates that element; using the next index appends an element. Skipped indices are
rejected rather than filled with empty entries. To replace the entire array instead, provide an
inline TOML value, such as
`-o 'server.routes=[{ path_prefix = "/api", backend = "app" }]'`. Override values use TOML
syntax, not PHP array syntax (`['k' => 'v']`) or JSON.

To serve only real files, use `--front-controller ''`. An empty `-o server.front_controller=` is
not valid because the empty value is reserved for that option.

When there is **no configuration file**, relative paths such as
`server.tls.certificate`, `[server.acme].cache`, and the access-log path resolve from the
working directory. `volucer check` shows the resolved paths.

If you explicitly pass `-c` with a path to a missing file, startup fails. Omitting a
configuration file is different from naming one that should exist.

By default, a request for a path that does not name a file is passed to the front
controller with `PATH_INFO` set. This is how many frameworks handle application routes.
Setting the front controller to an empty string disables that behavior: a path that does
not name a file returns 404, similar to nginx's `location ~ \.php$`.

## Check the version

```sh
volucer version
```

This prints the version and capabilities of your build. Include this output when
reporting a problem:

```
volucer 0.1.0
  front-ends:       HTTP/1.1, HTTP/2 (cleartext), FastCGI, TLS (rustls)
  worker protocol:  1
  php floor:        8.2
  backends:         worker, attach, external fpm, spawn_cgi, upstream
  certificates:     ACME (RFC 8555), the tls-alpn-01 challenge
  observability:    access log (combined or json), W3C tracecontext
  traces:           OTLP/HTTP export (POST /v1/traces, default port 4318)
```

## Check whether a running server is healthy

```sh
volucer health --config volucer.toml          # /health: is the process alive?
volucer health --config volucer.toml --ready  # /ready: should traffic be sent here?
volucer health --url http://127.0.0.1:2114/health
```

The command exits with `0` when the check passes and a nonzero status when it fails; it
prints failure details to stderr. This makes it suitable for a Docker `HEALTHCHECK`, even
in images without a shell or `curl`. See [Health checks](11-OPERATIONS.md#health-checks)
for details.

## What to read next

- **[Configuration](01-CONFIGURATION.md)** — all configuration keys and their defaults.
- **[Backends](02-BACKENDS.md)** — the five backend types and how to choose between them.
- **[Operations](11-OPERATIONS.md)** — deployment with nginx, systemd, or Docker, plus
  reloads without dropping connections.
