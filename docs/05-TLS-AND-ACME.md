# TLS and ACME

Configure HTTPS with certificates you provide or certificates issued automatically
through ACME.

## Turning TLS on

TLS is enabled by adding `[server.tls]`; there is no separate `enabled` option. When
managing certificates manually, both `certificate` and `key` are required. A table with
only one of them is invalid.

```toml
[server]
http = "0.0.0.0:443"

[server.tls]
certificate = "certs/default.crt"   # PEM chain, leaf first
key = "certs/default.key"           # PEM private key
min_version = "1.2"                 # or "1.3"; a floor, not a cap
alpn = ["h2", "http/1.1"]           # h2 preferred; do NOT drop http/1.1
plain = false                       # also answer cleartext on this same port
```

| Key | Type | Default | What it does |
|---|---|---|---|
| `certificate` | path | absent | PEM certificate chain, with the leaf certificate first. |
| `key` | path | absent | PEM private key. |
| `min_version` | string | `"1.2"` | Minimum TLS version: `"1.2"` or `"1.3"`. |
| `alpn` | array of strings | `["h2", "http/1.1"]` | Application protocols offered during negotiation. |
| `plain` | bool | `false` | Also accept cleartext HTTP on the TLS port. |

Relative paths for `certificate` and `key` resolve from the configuration file's
directory. This differs from `document_root`, which is relative to the process working
directory. Resolving certificates beside the configuration file makes their location
independent of where the process was started. `volucer check` displays the resolved paths.

Keep `http/1.1` in `alpn`. Volucer supports both HTTP/2 and HTTP/1.1, and some clients
offer only HTTP/1.1.

## Plain HTTP and TLS together

Volucer supports each of these listener configurations. The `http` setting retains its
existing meaning, so existing cleartext configurations continue to work unchanged.

| Shape | Configuration |
|---|---|
| Cleartext only | `http = "...:8080"`, no `[server.tls]` — **unchanged from before** |
| TLS only | `http = "...:443"` + `[server.tls]` |
| **Both on one port** | `[server.tls].plain = true` |
| Both, conventional ports | `http = "...:80"` and `https = "...:443"` |

When one port serves both protocols, Volucer inspects the first byte of the connection.
A TLS record begins with `0x16`; a cleartext request begins with an ASCII method letter.
The protocol is detected once per connection, not once per request.

This is protocol detection, not a downgrade mechanism. If a client starts a TLS handshake
and that handshake fails, Volucer does not retry the connection as cleartext. Since
accepting cleartext on a TLS port is a security-sensitive choice, `plain` is disabled by
default and its use is logged at startup.

HTTPS does not require a privileged port; `https://host:8443` uses the same TLS handshake
as the default HTTPS port. ACME HTTP-01 validation requires port 80, and a conventional
`:80` to `:443` redirect uses both standard ports.

### Redirecting to HTTPS

```toml
[server]
http = "0.0.0.0:80"
https = "0.0.0.0:443"
redirect_to_https = true

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"
```

`redirect_to_https` defaults to `false`. It requires a cleartext listener, either a
separate `https` listener or `plain = true`. Enabling it when TLS is configured on `http`
alone is an error because there would be no cleartext requests to redirect.

The redirect returns 301, preserves the path and query, and uses the configured `https`
port rather than the port that received the request. The default port, `443`, is omitted
from the authority.

Redirects apply only to cleartext connections. When `plain = true`, the same listener
accepts both protocols; redirecting its TLS connections would send clients back to the
same URL and cause a loop. Volucer makes the decision based on the connection protocol.

### Redirecting one site rather than all of them

`redirect_to_https` is also a **site** key, and a site that states nothing inherits the
deployment's answer:

```toml
[server]
redirect_to_https = true

[sites.corp]
hosts = ["internal.corp"]
addresses = ["192.168.9.123"]
protocols = ["http", "https"]
redirect_to_https = false     # deliberately cleartext on this address
```

Per-site configuration allows an internal site to remain cleartext while other sites
redirect to HTTPS. For example, a site bound to an internal address with
`protocols = ["http"]` may not have a certificate and should not redirect clients to a
scheme it does not serve.

Volucer rejects these combinations because the redirect could never work:

- `redirect_to_https = true` when the site's `protocols` excludes `"http"`. The site
  cannot receive cleartext requests to redirect.
- `redirect_to_https = true` when the site's `protocols` excludes `"https"`. The
  redirect would target a scheme the site does not serve.

Volucer resolves redirects using the selected site's settings. It checks the redirect
before doing further request processing; the request body has not been read at that point.

### `Strict-Transport-Security`

```toml
[server.hsts]
max_age_s = 300
include_subdomains = false

[sites.shop.hsts]
max_age_s = 31536000
include_subdomains = true
preload = true
```

Sites inherit the policy from `[server.hsts]`. A site's HSTS table overrides only the
directives it specifies, so unspecified values continue to inherit. If no HSTS policy is
configured, Volucer sends no HSTS header.

`max_age_s` is required in `[server.hsts]` because there is no higher-level value to
inherit. There is no `hsts = true` shortcut: browsers retain an HSTS policy until they
receive an updated policy over TLS, so choosing an unsuitable duration can have lasting
effects.

Set `enabled = false` on a site to disable a policy it would otherwise inherit. Removing
the site's HSTS table means "inherit," not "disable."

Volucer rejects `max_age_s = 0`. In an HSTS header, zero removes a stored policy, so it
does not represent an active policy to configure.

`preload = true` requires `include_subdomains = true` and a `max_age_s` of at least one
year (`31536000`), as required for preload-list inclusion.

HSTS is sent only over a secure transport, as required by RFC 6797 §7.2. On a
`plain = true` listener, Volucer sends it on TLS connections but not cleartext ones.
Redirect responses are cleartext and do not include the header. Error responses, such as
404s, do include it, just as successful responses do.

HSTS is not sent over FastCGI. The proxy terminates TLS and is responsible for response
headers; the `HTTPS` value it sends is not a property of Volucer's connection.

Volucer rejects HSTS configuration in these cases because the policy cannot be applied:

- A site without `hosts` (the address-only default-site shape). Browsers store HSTS by
  hostname, not IP address (RFC 6797 §8.3), so the policy would not apply.
- A site whose `protocols` excludes `"https"`, or a deployment without `[server.tls]`.
  Neither has a secure transport on which to send the header.

`volucer check` warns when one site's `include_subdomains` policy covers another site in
the same configuration. After visiting the parent, a browser will also require HTTPS for
the child hostname, even if it resolves to an internal address. HSTS applies to hostnames,
not addresses, so a policy follows the name wherever it resolves.

## Per-site certificates

```toml
[sites.shop.tls]
certificate = "certs/shop.crt"
key = "certs/shop.key"
```

Only `certificate` and `key` can be set per site. Volucer selects the certificate during
the TLS handshake, before an HTTP request exists. Options such as `min_version`, `alpn`,
and `plain` configure the listener, not an individual site's certificate; setting them
per site is an error.

Volucer selects certificates using SNI, not the HTTP `Host` header. It chooses an exact
name first, then the longest matching `*.` wildcard, and otherwise uses the server
certificate. SNI and `Host` can differ: Volucer presents the certificate selected by SNI,
then routes the request to the site selected by `Host`.

## What TLS costs when it is off

Without `[server.tls]`, Volucer does not inspect the connection for TLS or construct TLS
state. Cleartext requests use the same path as before TLS support was added.

## HTTP/3

HTTP/3 runs over QUIC, which is a different transport rather than a newer framing layer, so
it gets its own **UDP** listener. Everything above the transport is shared: site resolution,
the rate limit, static files, routing, the dispatcher, the access log and the metrics are
the same code that serves HTTP/1.1 and HTTP/2.

```toml
[server]
http = "0.0.0.0:443"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"
alpn = ["h2", "http/1.1"]      # the TCP socket's list -- see below

[server.http3]
listen = "0.0.0.0:443"         # UDP; the same port number as `http` is fine
```

| Key | Type | Default | What it does |
|---|---|---|---|
| `listen` | `host:port` | required | UDP address to bind. `unix:` forms are refused. |
| `max_connections` | integer | `512` | Concurrent QUIC connections. |
| `idle_timeout_ms` | integer | `30000` | Idle timeout, per RFC 9308. |
| `alt_svc_ma_seconds` | integer | `86400` | The `ma=` in the `Alt-Svc` header. |
| `max_field_section_size` | integer | `16384` | Largest request header section, in bytes. |

**It requires a build with the `http3` feature.** Published releases and the Docker images
carry it; a local build needs `cargo install volucer --features http3`. The configuration
table parses either way, and a build without the listener refuses it by name rather than
reporting an unknown field.

**It requires `[server.tls]`.** QUIC is TLS 1.3 with no cleartext mode anywhere in the
protocol, so there is no equivalent of `plain`. The certificate comes from the same
resolver, which means per-site certificates, ACME issuance and a certificate reload all
reach this listener unchanged — only `min_version` and `alpn` are restart-only, and for
HTTP/3 neither applies: the listener is always TLS 1.3 and its ALPN is `h3`.

**`h3` is not added to `[server.tls].alpn`,** and writing it there is an error. That list
belongs to the TCP socket, which cannot speak HTTP/3 at all: a client that negotiated `h3`
over TCP would send a QUIC Initial down a TCP connection. HTTP/3's ALPN is fixed by RFC
9114 §3.1.

### `Alt-Svc` is not optional

A browser has no way to discover a UDP endpoint. It learns one from `Alt-Svc` on a TCP
response:

```
Alt-Svc: h3=":443"; ma=86400
```

Volucer sends that automatically on every **TLS** answer when `[server.http3]` is present,
naming the port the socket actually bound. It is never sent on a cleartext answer, because
no browser acts on it there. Without the header the listener exists and is never used.

### `max_field_section_size` is a security setting

`h3` 0.0.8 defaults this to unbounded, which is hyperium/h3#379: a `HEADERS` frame made of
one-byte static-table references decodes into a `HeaderMap` with thousands of entries, so a
small request allocates a large one. RFC 9114 §4.2.2 charges each decoded field at least 32
bytes against this budget, so the 16 KiB default caps a section at roughly 512 fields and
the section is rejected *before* the fields are materialised.

Do not raise it above the 512 KiB ceiling that validation enforces. Do not return it to the
crate's default. The test
`an_oversized_header_section_is_answered_431_and_the_listener_survives` is what makes the
mitigation a mitigation rather than a comment.

**The ceiling is derived from a panic, not chosen for roundness.** `h3` 0.0.8 builds the
request's header map with `HeaderMap::with_capacity(fields.len())`, and that call panics above
`http`'s limit — measured, not assumed: `with_capacity(24576)` succeeds and
`with_capacity(24577)` aborts with `size overflows MAX_SIZE`. Because the budget bounds the
*field count* at 32 bytes each, a ceiling above 786 432 bytes would admit the very crash it
exists to prevent. 512 KiB caps the worst case at 16 384 fields, a third below the threshold.
`the_ceiling_cannot_admit_a_panicking_field_count` pins the arithmetic, so an `http` upgrade
that lowers the limit fails a test instead of a deployment. The real fix is hyperium/h3#378,
which replaces the panicking pre-allocation with `try_with_capacity`.

### What differs from the TCP path

**The request body is buffered, up to `max_body_bytes`.** A QUIC request stream is read by
the application, so there is no way to hand it across the blocking boundary the CGI path
uses. An HTTP/3 upload therefore meets the same limit a CGI request does, rather than
streaming to an origin unbounded the way an `upstream` request does over TCP. The
**response** still streams, so SSE and large downloads are unaffected.

**Response trailers cross this hop.** They are read from the response body as frames and
written with `send_trailers`, so a gRPC call's `grpc-status` reaches a QUIC client exactly as
it reaches an HTTP/2 one. This is asserted rather than assumed:
`a_grpc_call_over_http_3_reports_its_status_in_the_trailers` and
`an_h2c_origins_trailers_reach_an_http_3_client` cover the two ways a trailer section is
produced — constructed by Volucer from a worker's answer, and passed through from an origin.

**The version stated to an origin is not the version that arrived.** Each hop negotiates its
own protocol, and the client Volucer uses for an `upstream` backend speaks HTTP/1.0, 1.1 and
2 only — so a request that arrives over QUIC is forwarded as HTTP/1.1 (or HTTP/2 by the
connector, if the origin was configured for `h2c` or negotiated `h2` through ALPN). Forwarding
the inbound `HTTP/3` made every HTTP/3 request to an `upstream` backend fail as an unreachable
origin.

**0-RTT is not enabled.** It makes replay a property of the application, and an application
written for a request/response server is not prepared for that.

### Firewall and MTU

The QUIC port is UDP. A TCP-only rule on `:443` means `Alt-Svc` advertises an endpoint that
nothing can reach, and every browser's QUIC attempt silently falls back to TCP after a
retry delay — a slow site rather than an error. See `11-OPERATIONS.md`.

## Automatic certificates

Automatic certificate management is opt-in. Add a `[server.acme]` table to configure it;
without that table, Volucer's behavior is unchanged.

```toml
[server]
http = "0.0.0.0:443"           # the TLS port the challenge is validated on
document_root = "/srv/app"

[server.tls]                    # no certificate or key: one is generated in memory
                                # and replaced by the first issuance

[server.acme]
enabled = true                 # false = keep these settings, order nothing
domains = ["example.com", "www.example.com"]
contact = ["mailto:ops@example.com"]
# authority = "production"      # staging by default; see below
# cache = "acme-cache"          # relative to this file
```

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `true` | Issue certificates when enabled. |
| `authority` | string | `"staging"` | Select `"staging"` or `"production"`. |
| `domains` | array of strings | Derived from the site's hosts | Names to include in the certificate. |
| `contact` | array of strings | absent | Contact addresses for CA notifications about renewal failures. |
| `cache` | path | `"acme-cache"` | Directory for issued certificates and account data. |

The table's presence configures ACME; `enabled` controls whether Volucer uses those
settings to issue certificates. This allows you to retain settings while issuance is
disabled. Removing the table disables ACME and removes those settings.

### The certificate and key may be left out

`[server.tls]` is still required to enable TLS, but ACME allows its `certificate` and
`key` to be omitted. Volucer then generates an in-memory self-signed placeholder for the
configured `domains`. The first issued certificate replaces it. The placeholder is not
written to disk because it is not trusted.

If no certificate and key are configured and no ACME table is enabled to issue one,
startup fails. This also applies when the only ACME table has `enabled = false`.

An issued certificate replaces the certificate currently in use without a restart; new
TLS handshakes receive the updated certificate. `[server.tls]` remains required because
the TLS listener needs a certificate to serve while ACME validation is in progress.

### The challenge

Volucer uses the `tls-alpn-01` challenge. The `http-01` challenge requires serving
`/.well-known/acme-challenge/<token>`, a path the static layer refuses because it begins
with a dot. TLS-ALPN-01 uses the TLS handshake on the port the site already serves and
does not require port 80 or a dotfile policy change.

The TLS port must be publicly reachable, and each name must resolve to this machine.
Volucer does not support DNS-01 validation, which is commonly used for hosts behind NAT
and requires DNS API credentials on the server.

### Staging is the default

Staging is the default to help avoid production rate limits during initial setup. It uses
the ACME protocol but issues certificates that browsers do not trust.

Set `authority = "production"` to use the production service. The value is an enum rather
than a URL, reducing the chance of selecting production unintentionally.

### `enabled = false`

Retains the table and its settings without issuing certificates. This is useful for
staging configuration or temporarily pausing orders:

```toml
[server.acme]
enabled = false                  # defaults only: stage these, order nothing
contact = ["mailto:ops@example.com"]
authority = "production"
```

- Keep staging settings for sites that inherit them, or pause an order during an incident
  without deleting its configuration. A disabled table can omit `domains` because it
  does not create an order.
- `enabled` is not inherited from `[server.acme]` by a site. Disable issuance on each
  ACME table where you want it paused.

### The cache

Volucer caches issued certificates. On restart, a valid cached certificate is served
immediately without using the CA's issuance rate limit. By default, the cache directory
is `acme-cache` beside the configuration file; account data is stored there as well.

The cache contains the ACME account key and certificate private keys. On startup,
Volucer sets the directory permissions to `0700` and files in it to `0600`. It reapplies
these permissions to existing cache entries as well, including caches created by older
versions or manually. This restricts access to the service account on shared Unix hosts.
Windows uses ACLs inherited from the parent directory instead; a cache under a user's
profile is already private.

If the configured `cache` path exists as a file rather than a directory, startup fails
and reports the path. This identifies the problem before a later write fails during
certificate issuance.

```bash
kill -HUP $(pidof volucer)     # pick up a certificate you replaced by hand
volucer check --config volucer.toml
```

Use `volucer check` to monitor issuance. A failed renewal may not immediately affect
service because the previous certificate remains available, so check reports the
authority, configured names, whether a certificate has been cached, and the remaining
validity of each certificate in use. It also warns when production is selected.

The check also reports when no `contact` is configured. Contact details give the CA a
way to notify you about renewal failures.

## One certificate per site

Use a site-level ACME table to issue a separate certificate for a site:

```toml
[server]
http = "0.0.0.0:443"
document_root = "/srv/default"

[server.tls]

[server.acme]
domains = ["example.com", "www.example.com"]
contact = ["mailto:ops@example.com"]

[sites.corp]
hosts         = ["corp.internal", "*.corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
# domains omitted: derived from this site's hosts
```

If omitted, `domains` is derived from the site's `hosts`. Wildcard entries are excluded
and logged because a certificate must contain concrete names, not patterns such as
`*.corp.internal`. If no names remain, Volucer reports an error listing the wildcards
rather than submitting an empty request.

`authority`, `contact`, and `cache` are also optional. If omitted, they inherit the
deployment-level values. This allows sites to share a cache directory and, when they use
the same contact, an ACME account.

The contact list and ACME directory URL determine the account key; the account file is
named using a hash of those values. Sites with the same contact and authority therefore
share an account, while different contacts create separate accounts. This matters because
Let's Encrypt applies rate limits per account.

The site's issued certificate replaces the deployment certificate for that site's hosts
only. A site can also specify its own `[sites.corp.tls]` certificate and key; these are
used until ACME issues a replacement. This supports migration from a certificate managed
outside Volucer.

Each domain must belong to only one ACME table. Volucer rejects a name listed in both a
site table and `[server.acme]`, and identifies the conflicting tables. During validation,
the TLS handshake identifies the domain through SNI. If both tables tried to validate
the same name, the deployment's challenge could answer with a key authorization that
does not match the site's order.

| Use case | Configuration |
|---|---|
| One certificate for the deployment | `[server.acme]` |
| A certificate per site | `[sites.<name>.acme]`, optionally inheriting contact and cache settings |
| A certificate managed outside Volucer | `[sites.<name>.tls]` or `[server.tls]` |
| Retain settings without issuing certificates | Set `enabled = false` on the relevant table |

## ACME provider

Let's Encrypt is the only supported ACME provider. `authority` is an enum rather than a
URL, so select `"production"` explicitly when needed.
