# Getting Started

**Install volucer, serve an application, and move off `php-fpm`.**

## What volucer is

Volucer is a single binary that runs applications. It never embeds a runtime — `php`,
`php-cgi` and `php-fpm` are started as ordinary external processes, the same way any
other server would start them, and it runs any FastCGI server and speaks its own worker
protocol besides.

It offers two ways to run your application, and you pick per route:

| | What happens per request | Use it when |
|---|---|---|
| **Worker mode** | The application boots once and stays resident. It clears its own request scope between requests. | You can restructure the app to reset its own state, and the boot cost matters. |
| **FastCGI mode** | The backend resets everything, exactly as `php-fpm` does it. | You are migrating an existing app and cannot change how it handles state. |

Both are served by the same listener, so a single process can run a `/api` route in worker
mode and a `/admin` route in FastCGI mode.

**FastCGI is a protocol, not a PHP feature.** A FastCGI-speaking WSGI, Rack or Go server
is served exactly as `php-fpm` is: `external` attaches to a pool somebody else runs, and
`spawn_cgi` starts and supervises the processes itself — with `php-cgi` as one driver and
any other FastCGI server as another.

It runs on Linux, macOS and Windows. On Windows it is the only way to get `php-fpm`-like
behaviour, because Windows has no `php-fpm` at all.

## Requirements

- **PHP 8.2 or newer**, if your application is PHP. This is the floor for the PHP SDK.
- An application. Any framework works, in any language the backend supports.
- No Rust toolchain, no Composer package, no build step.

## Install

### Linux and macOS

```console
curl -fsSL https://github.com/sailantis/volucer/releases/latest/download/install.sh | sh
```

The binary lands in `~/.local/bin`.

### Windows

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/sailantis/volucer/releases/latest/download/install.ps1 | iex"
```

The binary lands in `%LOCALAPPDATA%\volucer\bin`, which the installer adds to your user
`PATH`. Open a new shell afterwards for that to take effect.

Both scripts resolve the newest release, download the archive for your platform, verify its
SHA-256 **before** extracting anything, and only then put the binary in place. A failure at
any step leaves your existing binary exactly as it was.

A SHA-256 fetched from the same server as the archive proves the archive was not corrupted in
transit; it cannot prove the archive came from this repository, because whoever can replace one
file can replace the other. Every release is therefore attested with
`actions/attest-build-provenance`, and both installers check that attestation when they can —
with `gh attestation verify`, against a signature GitHub's OIDC identity made and Sigstore's
transparency log records, which a compromised or hostile download origin cannot forge.

That check needs the GitHub CLI and a GitHub login, which a `curl | sh` cannot assume, so it is
best-effort by default and the installer says plainly which of the two guarantees you got. Set
`VOLUCER_VERIFY=require` (or `-Verify require` on PowerShell) to make it mandatory: the install
then refuses to proceed when the attestation cannot be checked. A *failed* verification is fatal
in every mode except `off`.

They read their settings from environment variables, because `curl | sh` and `irm | iex`
cannot pass arguments. Both scripts read the same names:

| Variable | Effect |
|---|---|
| `VOLUCER_VERSION` | Install a specific tag instead of the newest release. |
| `VOLUCER_INSTALL_DIR` | Where to put the binary. |
| `VOLUCER_FORCE=1` | Replace an existing binary. |
| `VOLUCER_DOWNLOAD_URL` | A mirror, a GitHub Enterprise host, or an air-gapped host. |
| `VOLUCER_API` | The release API root. |
| `VOLUCER_VERIFY` | `auto` (default), `require` or `off` — whether to check the build provenance attestation. |

`install.ps1` takes the same settings as parameters (`-Version`, `-InstallDir`, `-Force`,
`-BaseUrl`, `-Verify`) and falls back to the environment variables, so a one-liner that can only
set environment variables behaves the same as one that passes parameters.

The shell script also honours `VOLUCER_LIBC` (`gnu` or `musl`) and `VOLUCER_ARCH`
(`x86_64` or `aarch64`).

### In a container

`packaging/docker/Dockerfile` builds two images from one crate — a glibc image and a
`scratch`-based musl image:

```console
docker build --target musl -t volucer .
```

### Directly

Download the archive for your platform from the
[releases page](https://github.com/sailantis/volucer/releases), verify it against the
`.sha256` file beside it, and put the binary on your `PATH`. That is the whole procedure.

## Check your setup

Before serving anything, ask volucer what it found on this host:

```console
volucer check
```

This parses your configuration, resolves the document root and front controller the way
`serve` would, discovers the PHP binaries it can find, and reports which of them are
executable. It is the fastest way to find out why a deployment is not doing what you
expected.

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

Read the `warnings:` block if one appears — it is where a path that does not exist, a front
controller the document root does not contain, or a worker `command` naming a program that
is not on `PATH` is reported. A warning does not make `check` fail: `configuration OK` means
the configuration is *internally consistent*, and a warning is a statement that it may not
describe the deployment you meant. Exit code `0` with three warnings is a deployment that
will start and misbehave.

`volucer check` needs a configuration file to check. If you do not have one yet, write
the minimum below first.

If volucer cannot find your PHP, point it at the directory:

```console
volucer check --php-dir /usr/local/php/bin
```

## Write a configuration file

Volucer reads a TOML file called `volucer.toml` by default. This is the smallest useful
one:

```toml
[server]
http = "127.0.0.1:8080"
document_root = "./public"
default_backend = "app"

[backends.app]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"
```

Three things are in there:

- **`[server]`** — the listeners, where your application lives, and which backend answers a
  request that matches no route.
- **`[backends.app]`** — how the application gets run. This one attaches to a `php-fpm` that is
  already running, so the behaviour is exactly what you have today.
- **`default_backend`** — the backend used when a request matches no route. It is one of
  `[server]`'s keys rather than a top-level one, because `[server]` **is** the unnamed site:
  a `[sites.<name>]` table overrides it for one host, exactly as it overrides `document_root`.

That is a complete FastCGI deployment. It behaves like `nginx` in front of `php-fpm`,
except there is no `nginx` to configure. Any FastCGI server is the same backend — a `socket`
is a socket, and the protocol is FastCGI:

```toml
[backends.app]
kind = "external"
socket = "unix:/run/app/py.sock"
```

To use volucer's own worker mode instead, replace the backend:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
```

That needs a `worker.php` in your application — see
[Worker Applications](07-WORKER-APPLICATIONS.md).

## Serve

```console
volucer serve
```

Your application is now on <http://127.0.0.1:8080>.

`volucer serve` accepts a few options:

| Option | Effect |
|---|---|
| `-c`, `--config <PATH>` | The configuration file. Defaults to `volucer.toml`. |
| `--document-root <PATH>` | Override `server.document_root`. |
| `--front-controller <NAME>` | Override `server.front_controller`. Set it to an empty string to serve only real files. |
| `--max-connections <N>` | Maximum concurrent client connections. Defaults to `512`. |

A flag always wins over the file. That is deliberate: a systemd unit or a container image
can describe its whole deployment in one file and not carry arguments.

`--front-controller ''` is worth knowing about. By default a path that names no file is
handed to the front controller with `PATH_INFO` set, which is what keeps a framework
working. Setting it empty turns that off and makes volucer behave like nginx's
`location ~ \.php$` — a path naming no file is a 404.

## Check the version

```console
volucer version
```

This prints the build's capabilities, which is a useful thing to have in a bug report:

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

## What to read next

- **[Configuration](01-CONFIGURATION.md)** — every key, with its default.
- **[Backends](02-BACKENDS.md)** — choosing between the five ways to run PHP. This is
  the decision that matters most.
- **[Operations](11-OPERATIONS.md)** — running behind nginx, systemd, Docker, and
  reloading without dropping connections.
