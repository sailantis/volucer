# Routing and Sites

**Send different paths to different backends, and serve several applications from one
process.**

## Routes

A route selects a backend. One process can serve a worker-backed API and a
`php-cgi`-backed legacy area at the same time.

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
| `path_prefix` | Paths starting with this. |
| `path_suffix` | Paths ending with this. |
| `host` | This `Host` header. |
| `policy` | `"all"`, `"backend"` or `"static"`. Default `"all"`. |
| `backend` | The backend for matched requests. Required unless `policy = "static"`. |

### Which route wins

The **most specific** matching route wins, and ties go to the earlier entry. "Most
specific" is three sentences rather than a weight, because a table whose winner could not
be predicted by reading the file would be worse than a simple one:

1. a route stating **more** constraints beats one stating fewer;
2. at the same count, an **exact** `host` beats a `*.` wildcard;
3. anything still tied goes to the route declared **first**.

### `path_suffix`

Compared **case-insensitively**, deliberately: the router decides what a script is the
same way, so `/upload.PHP` cannot be a script to one layer and miss its route in the
other.

It must start with `.` or `/` — `"php"` would match `/notphp`, which is not what it looks
like it does, so it is refused.

### Several PHP entry points

`.php` files are resolved to real scripts, so `path_suffix = ".php"` sends *every* PHP
entry point to one backend. That is the usual answer for a project with more than one
front controller:

```toml
[[server.routes]]
path_prefix = "/api"                # a framework API on the persistent worker pool
backend = "app"

[[server.routes]]
path_suffix = ".php"                # each real script on the CGI pool
backend = "cgi"
```

A **worker** backend is told each request's `script_filename` but does not switch file on
it: it routes on `REQUEST_URI`. So an entry script reached through a worker gets its real
path in `$_SERVER`, and the worker's own bootstrap decides what to run — correct for one
application per site, and wrong for several independent scripts that each need their own
bootstrap. Use `spawn_cgi` for those.

### The `stat` a route can skip

Because a route's `path_suffix` can prove that a path names a script, such a route
**skips the router's own `stat`**: `php-cgi` checks existence itself and answers `No input
file specified`.

A route that proves nothing about the extension — a bare `path_prefix`, or a path to
`php-cgi` whose pattern could name `config.json` — keeps the check, because `php-cgi` does
not verify that a file is PHP: given a non-PHP file it emits it verbatim with a `200`.

`volucer check` prints which routes skip it, so the saving and the guard are both visible.

### `policy`

| Value | Behaviour |
|---|---|
| `"all"` | Static files first, then the route's backend. The default. |
| `"backend"` | The route's backend only. A path naming a file is dispatched like any other. |
| `"static"` | Files only. A path naming no file is a 404, and no backend is consulted. |

`policy = "static"` is refused when the route names no backend *and* the site has no
`default_backend`, because there would be nothing for `/assets/missing.png` to be a 404
*instead of*.

## Sites

One process can serve several hosts, each with its own document root.

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

This exists because a `host` matcher in the route table is consulted **after** the script
is resolved, so it can choose a backend but never a document root. Sites are resolved
first, which is what makes two roots possible in one process.

### How a host is matched

- `hosts` takes exact names, case-insensitively and ignoring any `:port`, or a leading `*.`
  wildcard.
- `*.example.com` matches `a.example.com` and `a.b.example.com`, but **not**
  `example.com` itself — a wildcard names a subdomain.
- The rule is: an exact name, then the **longest** matching wildcard, then `[server]`. A
  host nobody claims is the default site's.

### Binding a site to addresses and protocols

`hosts` alone is a **client's** statement: the `Host` header is whatever the caller typed, so
a site claiming `internal.corp` is reachable by anyone who can reach the port and send that
name. Two further keys make the decision depend on facts about the connection instead.

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
| `addresses` | The **local** addresses this site answers on. Absent means any. |
| `protocols` | `"http"`, `"https"`, `"fastcgi"`. Absent means all. |

`addresses` is the **destination** of the connection — the address the socket is bound to —
not the client's address. That is the whole point: `REMOTE_ADDR` is a client's claim once a
proxy is in front, but the local address is what the kernel accepted the connection on, and a
client cannot rewrite it. A request to `135.181.172.123` can therefore not select a site bound
to `192.168.9.123`, whatever `Host` it sends.

Entries are addresses or CIDR ranges, parsed by the same code as
`[server.proxy].trusted_subnets`, so a range means the same thing in both places:

- `192.168.9.123` — one address, the usual case.
- `192.168.9.0/24` — a whole interface.
- `::1`, `fd00::/8` — IPv6. **A v4-mapped v6 address is not a v4 address**, so a listener on
  `[::]` that accepts `::ffff:192.168.9.123` does not select a site bound to
  `192.168.9.123`. Bind the listener to `0.0.0.0`, or write the mapped range out.

`protocols` is the **front-end** that received the request, not `X-Forwarded-Proto`:
`https` means "arrived on a TLS-speaking listener", so a site restricted to `https` does not
answer the cleartext half of a `[server.tls] plain = true` port. Over FastCGI it means the
proxy set `HTTPS`.

### Which site wins

The order is what makes the binding worth writing — the first two rules cannot be influenced
by what a client sends:

1. A site that names `protocols` must admit the protocol the request arrived over.
2. A site that names `addresses` must admit the local address it arrived on.
3. Among the sites that survive, the host rule applies unchanged — exact, then longest
   wildcard — **to the ones that survive**. A name claimed only by an ineligible site falls
   through to a less specific pattern that can serve this connection, then to an address-only
   site, and finally to `[server]`. It never falls back to the ineligible site.
4. An **address-only** site — one with `addresses` and no `hosts` — is the last resort, and
   the narrowest range that admits the address wins. This is nginx's `default_server` shape:
   one application on one internal address, with no hostname to invent for it.

```toml
[sites.internal]
addresses     = ["192.168.9.123"]    # no `hosts` at all
document_root = "/srv/internal"
```

Such a site answers **every** request that reaches that address, including one whose `Host`
names nothing configured. That is intended, and it is why the key is worth writing: the
address is the part of the request an attacker does not choose.

Two address-only sites may not claim the same address — which one won would depend on
declaration order, and neither the file nor a reader could predict it.

### A note on `check`

`volucer check` prints each site's `hosts`, `addresses` and `protocols`, so what a site is
actually bound to is visible without reading the file. It prints the **resolved** HSTS
policy in the header form a client receives, rather than the keys a site happened to write —
a site that inherits is the ordinary case, and "what header does this name actually get" is
the question the line exists to answer.

### What a site inherits

Every key except `hosts` and `document_root` falls back to the `[server]` value, so a site
that states only a root and a host list inherits the rest.

`static_files` falls back **per key**: a site naming `precompressed` keeps the server's
`cache_control`. The same is true of `compression`, `acme` and `hsts`.

A site's `routes` **replaces** `[server.routes]`. It is not merged, so a host that states
one route does not inherit the other twenty.

`default_backend` inherits the same way and is what `/assets/missing.png` falls to when a
`policy = "static"` route names no backend of its own.

`redirect_to_https` inherits too, and is the one inherited key that is a *behavioural*
default rather than a setting: a site bound to an internal address may be deliberately
cleartext, so it can turn off a redirect it would otherwise inherit. See
[TLS and ACME](05-TLS-AND-ACME.md).

### `front_controller = false`

Means "serve only real `.php` files". The global key cannot say this — TOML cannot tell
`front_controller = ""` from an absent key, so that spelling stays reserved for the
`--front-controller ''` flag — but a site can, because there is no flag per site.

### `serve_only`

Says what the site serves:

| Value | Behaviour |
|---|---|
| unset | Everything: static first, then a backend. |
| `"backend"` | Skips the static layer. |
| `"static"` | A path naming no file is a 404, and no backend is consulted. |

`static_files = false` is exactly `serve_only = "backend"`.

### A note on the command-line flags

`--document-root` and `--front-controller` describe the **default** site, so combining
either with `[sites]` is refused rather than silently applying to one site.

### Checking it

`volucer check` prints the site table, what each site states, and which routes skip the
stat.
