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

**Restart-only, and reported on every reload** rather than left to be discovered — a setting
the implementation cannot honour should be *stated*, or the operator finds out from
behaviour:

| Setting | Why it cannot be reloaded |
|---|---|
| `http`, `https`, `fastcgi` | A bound socket cannot move |
| Backends, their commands and worker counts | Those are running processes with live pipes |
| `[server.tls]` `min_version` and `alpn` | They are baked into the `rustls` server config the acceptor holds |
| `[server.proxy]` | Whose forwarded headers are believed is a statement about this deployment's topology |
| `[server.rate_limit]` | The token buckets are live state, so swapping the policy would reset every caller's allowance |
| `[server.access_log]` path, format, `request_id_header` | Chosen at startup. **The log file is reopened on every `SIGHUP` regardless** |
| `[status]` address and `allow_worker_reset` | The listener is bound, and a route that recycles workers must not appear because somebody edited a file |

### A reload is whole-or-nothing

A file that does not parse, or does not validate, is refused with a logged reason and the
previous configuration keeps serving. There is no way to apply half of one — including when
the bad file names settings the server *can* apply.

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

`SIGTERM` drains: the listeners stop, in-flight requests finish, and the pool's PHP children
are killed with their parent rather than orphaned.

The ACME cache lives under the state directory, so `[server.acme] cache` and the unit's
`StateDirectory` have to agree. The pair the unit ships with is:

```toml
[server.acme]
cache = "/var/lib/volucer/acme-cache"
```

## Docker

`packaging/docker/Dockerfile` builds two images from the one crate — a glibc image and a
`scratch`-based musl image:

```console
docker build --target musl -t volucer .
```

The musl image is the smaller of the two and has no libc dependency, which matters for a
`scratch` base. There is no archive to fetch and nothing to verify by hand, because the
binary is built into the image.

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

## Health checks

The `[status]` listener answers two probes, and the difference between them is the one an
orchestrator acts on:

```toml
[status]
address = "127.0.0.1:2114"
```

```console
curl -sf http://127.0.0.1:2114/health   # is the process running?
curl -sf http://127.0.0.1:2114/ready    # should traffic be sent here?
```

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

1. Read the release notes for the version you are moving to.
2. Install the new binary alongside the old one, or replace it.
3. Run `volucer check --config volucer.toml` with the new binary. It reports what the new
   build resolves, and refuses a configuration the new build will not accept.
4. Restart the service.

A configuration that the new build refuses is refused **at startup**, not silently
reinterpreted — the configuration model is strict, so a removed or renamed key stops the
server rather than being ignored.
