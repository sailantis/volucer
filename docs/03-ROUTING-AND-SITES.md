# Routing and Sites

Routes direct requests to backends, while sites let one Volucer process serve multiple
applications with separate document roots and host, address, or protocol constraints.

## Routes

A route selects the backend for a matching request. For example, one Volucer process can
serve an API through a worker and send legacy PHP requests to `php-cgi`.

```toml
[server]
http = "127.0.0.1:8080"
default_backend = "app"

[[server.routes]]
path_prefix = "/api"        # a textual prefix, like nginx's `location`
backend = "app"

[[server.routes]]
path_suffix = ".php"        # every PHP entry point, wherever it lives
backend = "cgi"

[[server.routes]]
host = "admin.example.com"  # a bare name, or `*.example.com`; no port, case-insensitive
path_prefix = "/"
backend = "fpm"
```

| Key | What it matches |
|---|---|
| `path_prefix` | Matches paths that start with this value. |
| `path_suffix` | Matches paths that end with this value. |
| `host` | Matches the request's `Host` header. |
| `policy` | Selects `"all"`, `"backend"`, or `"static"` behavior. Defaults to `"all"`. |
| `backend` | Selects the backend for matching requests. Required unless `policy = "static"`. |

### Which route wins

The most specific matching route wins. Specificity is determined by these rules, in
order:

1. A route with more matching constraints takes precedence over one with fewer.
2. If both have the same number of constraints, an exact `host` takes precedence over a
   `*.` wildcard.
3. If they are still tied, the route declared first wins.

### `path_suffix`

Suffix matching is case-insensitive, consistent with script resolution. For example,
`/upload.PHP` matches a route with `path_suffix = ".php"`.

The value must start with `.` or `/`. A value such as `"php"` would also match paths like
`/notphp`, so Volucer rejects it.

### Several PHP entry points

Volucer resolves `.php` files to real scripts. A route using `path_suffix = ".php"`
therefore sends all PHP entry points to one backend, which is useful when an application
has multiple front controllers:

```toml
[[server.routes]]
path_prefix = "/api"                # a framework API on the persistent worker pool
backend = "app"

[[server.routes]]
path_suffix = ".php"                # each real script on the CGI pool
backend = "cgi"
```

A `worker` backend receives each request's `script_filename`, but it does not use that
value to select a script; it dispatches using `REQUEST_URI`. The worker can inspect the
resolved path in `$_SERVER` and let its own bootstrap decide what to run. This works well
for one application per site, but not for independent scripts that each require their own
bootstrap. Use `spawn_cgi` for that arrangement.

### The `stat` a route can skip

When a route's `path_suffix` establishes that a request targets a script, Volucer skips
its own filesystem `stat`. The backend, such as `php-cgi`, checks that the script exists
and reports `No input file specified` if it does not.

Routes that do not establish a script extension keep the check. For example, a
`path_prefix` could match `config.json`, and `php-cgi` does not verify that a target file
is PHP; it may return a non-PHP file as a successful response.

`volucer check` reports which routes skip the `stat`.

### `policy`

| Value | Behaviour |
|---|---|
| `"all"` | Try static files first, then use the route's backend. This is the default. |
| `"backend"` | Use only the route's backend; matching files are dispatched like any other path. |
| `"static"` | Serve files only. Return 404 when no file matches, without consulting a backend. |

Volucer rejects `policy = "static"` if the route has no backend and the site has no
`default_backend`. In that configuration, there is no backend to handle requests outside
the route, including a missing file such as `/assets/missing.png`.

## Sites

Sites allow one process to serve multiple hosts, each with its own document root and
settings.

```toml
[server]
http             = "127.0.0.1:8080"
document_root    = "/srv/default/public"   # the fallback site
front_controller = "index.php"
default_backend  = "app"                   # the unnamed site's routing

[backends.app]
kind    = "worker"
command = "php worker.php"

[backends.cgi]
kind    = "spawn_cgi"
binary  = 'C:\php-8.3.31\php-cgi.exe'

[sites.example_com]
hosts            = ["example.com", "www.example.com", "*.shop.example.com"]
document_root    = "/srv/example/public"

[sites.corp]
hosts            = ["corp.internal", "*.corp.internal"]
document_root    = "/srv/corp"
front_controller = false                   # files-only: no front controller
serve_only       = "backend"               # no static layer
static_files     = { precompressed = true }

  routes = [
      { path_suffix = ".php", backend = "cgi" },
  ]
```

Route host matching happens after script resolution. A route can select a backend, but
cannot select the document root used to resolve the script. Volucer selects a site first,
so each host can use a separate document root.

### How a host is matched

- `hosts` accepts exact names, matched case-insensitively with any `:port` ignored, or
  names beginning with `*.`.
- `*.example.com` matches `a.example.com` and `a.b.example.com`, but not `example.com`.
  The wildcard matches subdomains only.
- Volucer prefers an exact name, then the longest matching wildcard, then `[server]`.
  Requests for an unclaimed host use the default site.

### Binding a site to addresses and protocols

Host matching uses the request's `Host` header, which a client can choose. For example,
any client that can reach the listener can send `Host: internal.corp`. `addresses` and
`protocols` add constraints based on the connection itself.

```toml
[server]
http          = "0.0.0.0:8080"       # one socket, several addresses
document_root = "/srv/public"
front_controller = "index.php"

[sites.internal]
hosts     = ["internal.corp"]        # the name, for convenience
addresses = ["192.168.9.123"]        # ...but only on this address
document_root = "/srv/internal"

[sites.admin]
hosts     = ["admin.example.com"]
addresses = ["192.168.9.0/24"]
protocols = ["https"]                # and only over TLS
document_root = "/srv/admin"
```

| Key | What it does |
|---|---|
| `addresses` | Local destination addresses on which the site can serve requests. If absent, any address is allowed. |
| `protocols` | Allowed front-end protocols: `"http"`, `"https"`, or `"fastcgi"`. If absent, all are allowed. |

`addresses` refers to the connection's destination address, not the client's address. It
is the local address on which the connection was accepted and cannot be changed by the
client. For example, a request sent to `135.181.172.123` cannot select a site restricted
to `192.168.9.123`, regardless of its `Host` header.

Values can be individual addresses or CIDR ranges. They use the same parsing rules as
`[server.proxy].trusted_subnets`:

- `192.168.9.123` matches one address.
- `192.168.9.0/24` matches the range.
- `::1` and `fd00::/8` are IPv6 examples. A v4-mapped IPv6 address is not treated as an
  IPv4 address. For example, a listener on `[::]` that accepts
  `::ffff:192.168.9.123` does not match a site restricted to `192.168.9.123`. Bind the
  listener to `0.0.0.0`, or configure the mapped address range explicitly.

`protocols` refers to the front-end protocol that received the request, not
`X-Forwarded-Proto`. `https` means the request arrived through a TLS listener. Thus, a
site restricted to `https` does not handle the cleartext side of a listener configured
with `[server.tls] plain = true`. For FastCGI requests, the proxy's `HTTPS` setting
determines the protocol.

### Which site wins

Site selection applies connection constraints before host matching:

1. A site that specifies `protocols` must allow the protocol used by the request.
2. A site that specifies `addresses` must allow the local destination address.
3. Host matching then selects the exact match, or the longest matching wildcard, among
   the eligible sites. If a host matches only an ineligible site, selection falls through
   to a less specific eligible host pattern, then an address-only site, and finally
   `[server]`.
4. An address-only site (one with `addresses` but no `hosts`) is a fallback. If multiple
   address-only sites match, the narrowest matching range wins. This supports a default
   application for a specific internal address without requiring a hostname.

```toml
[sites.internal]
addresses     = ["192.168.9.123"]    # no `hosts` at all
document_root = "/srv/internal"
```

An address-only site handles every request received on that address, including requests
with an unconfigured `Host` value. This is intentional: the client cannot choose the
local address on which its connection is received.

Two address-only sites cannot claim the same address. Otherwise, the selected site would
depend on declaration order.

### A note on `check`

`volucer check` displays each site's `hosts`, `addresses`, and `protocols`. It also shows
the resolved HSTS policy in the form of the response header a client receives, including
values inherited from `[server]`.

### What a site inherits

Every site setting except `hosts` and `document_root` falls back to its `[server]` value.
A site can therefore specify only its host list and document root while inheriting the
remaining settings.

`static_files` inherits per key. For example, a site can set `precompressed` and inherit
the server's `cache_control`. The same per-key behavior applies to `compression`, `acme`,
and `hsts`.

A site's `routes` replaces `[server.routes]`; the two route lists are not merged. Once a
site defines routes, it does not inherit any routes from `[server]`.

`default_backend` also inherits from `[server]`. It handles requests not assigned a
backend by a `policy = "static"` route.

`redirect_to_https` is inherited as well. A site bound to an internal address may be
intended to serve cleartext, so it can disable a redirect inherited from `[server]`. See
[TLS and ACME](05-TLS-AND-ACME.md).

### `front_controller = false`

This setting makes a site serve only real `.php` files, without a front controller. The
global setting cannot use this value: an empty TOML string is treated like an absent key.
For the default site, use `--front-controller ''`; individual sites can set
`front_controller = false` directly.

### `serve_only`

Controls whether the site serves static files, backend responses, or both:

| Value | Behaviour |
|---|---|
| unset | Serve static files first, then dispatch to a backend. |
| `"backend"` | Skip static-file handling and use a backend. |
| `"static"` | Serve files only; return 404 if no file matches, without consulting a backend. |

`static_files = false` has the same effect as `serve_only = "backend"`.

### A note on the command-line flags

`--document-root` and `--front-controller` apply to the default site. Volucer rejects
these flags when `[sites]` is configured, avoiding ambiguity about which site they
should affect.

### Checking it

`volucer check` displays the configured sites, their effective settings, and which
routes skip the filesystem `stat`.
