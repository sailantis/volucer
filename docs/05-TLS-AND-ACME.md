# TLS and ACME

**HTTPS, with a certificate you supply or one that is issued for you.**

## Turning TLS on

`[server.tls]` is what turns TLS on — there is no `enabled` key, because a flag and a
certificate are two statements about one thing and two statements can disagree. A table
without both paths is an error rather than a defaulted half.

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
| `certificate` | path | absent | The PEM certificate chain, leaf first. |
| `key` | path | absent | The PEM private key. |
| `min_version` | string | `"1.2"` | `"1.2"` or `"1.3"`. A floor, not a cap. |
| `alpn` | array of strings | `["h2", "http/1.1"]` | The protocols offered. |
| `plain` | bool | `false` | Also accept cleartext on the TLS port. |

A relative `certificate`/`key` resolves against **the directory holding the config file**.
That is a deliberate exception to `document_root`'s rule, which is relative to the
process's working directory: a certificate beside `volucer.toml` is the usual case, and
resolving it against wherever the process started would be a footgun with a security
consequence. `volucer check` prints the path it resolved.

Do not drop `http/1.1` from `alpn`. The server can serve both, and a client that offers
only HTTP/1.1 is a client you want to serve.

## Plain HTTP and TLS together

All four shapes work. `http` always keeps its name and meaning, so no existing
configuration changes behaviour.

| Shape | Configuration |
|---|---|
| Cleartext only | `http = "...:8080"`, no `[server.tls]` — **unchanged from before** |
| TLS only | `http = "...:443"` + `[server.tls]` |
| **Both on one port** | `[server.tls].plain = true` |
| Both, conventional ports | `http = "...:80"` and `https = "...:443"` |

**One port carrying both** works by looking at the first byte: a TLS record always begins
with `0x16`, and a plaintext request begins an ASCII method letter. This is what nginx's
`ssl_preread` does, and it costs one peek per *connection* — never per request, because the
protocol is decided once and then the connection is one path or the other.

It is a **detection, not a downgrade**: a client that offers TLS and fails the handshake
still fails. There is no fallback to cleartext on a handshake error. And because accepting
cleartext on a port that clients believe is encrypted is a security decision, `plain` is off
by default and is logged loudly at startup when on.

No port is privileged either: `https://host:8443` sends the identical ClientHello. Only
two things genuinely need a fixed port, and both are `https`'s job — **ACME HTTP-01** (the
challenge server always contacts `:80`) and an **`:80 → :443` redirect**.

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

`redirect_to_https` is off by default because a redirect is a deployment's decision. It
needs something actually serving cleartext to fire on — a separate `https`, or
`plain = true` — so setting it with TLS on `http` alone is an error rather than a silently
inert key.

The redirect answers `301`, keeps the path and query, and names the `https` port rather
than the port the request arrived on; `:443` is left out of the authority because it is the
default.

**It applies to cleartext connections only.** With `plain = true` one listener carries both
protocols, and a redirect on the TLS half would send the client back to where it just came
from — an infinite loop. The decision is per connection for exactly that reason.

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

This is the case a single deployment-wide boolean cannot express. A site bound to an
internal address with `protocols = ["http"]` is *deliberately* cleartext — it has no
certificate for that name and no reason for one — and redirecting it would send its clients
to a scheme it does not serve.

Two combinations are refused as inert rather than accepted:

- `redirect_to_https = true` on a site whose `protocols` excludes `"http"` — such a site
  never receives a cleartext request to redirect.
- `redirect_to_https = true` on a site whose `protocols` excludes `"https"` — the redirect
  would point at a scheme this site does not answer.

The redirect is resolved **from the site**, so it is decided after site selection rather
than before it. It stays ahead of everything that costs work: a redirect is one host probe
and a comparison, and the body has not been read at that point.

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

`[server.hsts]` is what a site inherits; a site's own table names only the directives it
changes, so the example above keeps the server's `includeSubdomains` where it does not
override it. **Absent means no header at all**, the same rule `redirect_to_https` follows.

`max_age_s` is required on `[server.hsts]`, because there is nothing above it to inherit
from. There is deliberately no bare `hsts = true`: a policy nobody chose is a pin a browser
keeps enforcing until the name is reached again *over TLS*, so a wrong duration is not
undone by editing the file.

`enabled = false` on a site turns off a policy it would otherwise inherit. That is why the
switch is a key rather than the table's presence — removing the table means "inherit".

`max_age_s = 0` is refused: in the header it means "delete this policy", so writing it in a
table would be two statements about one thing.

`preload = true` requires `include_subdomains = true` and at least a year
(`31536000`), which is what the preload list itself enforces.

**Sent only over a secure transport.** RFC 6797 §7.2 forbids the header on a non-secure
transport, so a `plain = true` port sends it on the TLS half and never on the cleartext one,
and a redirect answer — cleartext by definition — does not carry it. It **is** sent on error
answers: a policy belongs to the name, so a 404 tells the browser about it just as a 200
does (nginx's `add_header … always`).

**Not sent over FastCGI.** The proxy in front terminates TLS and writes its own response
headers; `HTTPS` is the proxy's claim rather than a fact about this server's socket.

Two placements are refused because the header could never be honoured:

- A site with no `hosts` — the address-only `default_server` shape. A browser stores a policy
  against a **host name** and never against an IP (RFC 6797 §8.3), so every client that could
  reach such a site would ignore it.
- A site whose `protocols` excludes `"https"`, and a deployment with no `[server.tls]` at
  all: there is no secure transport to send it on.

`volucer check` warns when `include_subdomains` on one site covers another site in the same
file — a browser that has visited the parent then refuses cleartext to the child, including
an internal address it resolves to. HSTS is deliberately *not* address-bound: a policy set on
a public address applies to the name wherever it resolves.

## Per-site certificates

```toml
[sites.shop.tls]
certificate = "certs/shop.crt"
key = "certs/shop.key"
```

Only `certificate` and `key` can be per site. The name is claimed in the TLS handshake,
before any request exists, so a per-site table can only be about *which certificate is
presented* — `min_version`, `alpn` and `plain` describe the socket, and setting one per
site is an error rather than a key that does nothing.

Certificates are selected by **SNI**, not by the `Host` header: exact name, then the
longest `*.` wildcard, then the server's certificate. The two *should* agree but need not —
a client may claim one name in the handshake and another in the request. That is not an
error: the certificate for the claimed name is presented, and the request is then served by
whatever site its `Host` selects.

## What TLS costs when it is off

Nothing measurable. With no `[server.tls]` there is no peek, no branch, and no TLS value is
constructed — the cleartext path is the same code it was before TLS existed.

## Automatic certificates

Opt in with a `[server.acme]` table. Without one, nothing about the server changes.

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
| `enabled` | bool | `true` | Issue certificates. |
| `authority` | string | `"staging"` | `"staging"` or `"production"`. |
| `domains` | array of strings | derived from the site's hosts | The names to issue for. |
| `contact` | array of strings | absent | Addresses the CA warns about failing renewals. |
| `cache` | path | `"acme-cache"` | Where issued certificates are kept. |

`enabled` and the presence of a table are two statements rather than one, and the reason is
the thing a boolean cannot express: *"keep these settings and do not use them yet"*.
Removing the table is how you turn ACME off, and it throws away a contact and an authority
that would have to be retyped correctly afterwards.

### The certificate and key may be left out

`[server.tls]` stays required — it is what turns TLS on — but its `certificate` and `key`
may be **left out entirely** when ACME is configured. A self-signed placeholder is then
generated **in memory** for the names in `domains`, and the first issuance replaces it.
Nothing is written to disk: the placeholder is never trusted, so a private key on disk for
it would be a liability with no benefit.

A pair that is not named and no ACME table that **issues** is an error, so a deployment
cannot end up serving a placeholder for ever by omission — including by setting the only
table it has to `enabled = false`.

An issued certificate **replaces the one being served, without a restart**. The next
handshake gets the new certificate. That is also why `[server.tls]` is still required
rather than optional: a TLS listener with nothing to serve fails every handshake that is
not a challenge.

### The challenge

**Only the `tls-alpn-01` challenge is used**, and not by preference: `http-01` serves
`/.well-known/acme-challenge/<token>`, a path this server's own static layer refuses
because it starts with a dot. TLS-ALPN-01 needs no `:80` and no dotfile policy change —
the handshake goes to the same `:443` the site already uses.

It does require the **TLS port to be publicly reachable** and the name to resolve to this
machine. There is no DNS-01 support: that is the escape hatch for a machine behind a NAT,
and it puts DNS API credentials on the web server.

### Staging is the default

A production account has per-week rate limits, and a first deployment that reached
production by omission would spend them on its own mistakes. Staging runs the real
protocol and issues certificates no browser trusts.

`authority = "production"` has to be named. The value is an enum rather than a URL, so
production cannot be typed by accident.

### `enabled = false`

Keeps the table and its settings and issues nothing. Two things it is for:

```toml
[server.acme]
enabled = false                  # defaults only: stage these, order nothing
contact = ["mailto:ops@example.com"]
authority = "production"
```

- **Staging settings** for sites that inherit them, or **parking an order** during an
  incident, without deleting values that would have to be retyped correctly afterwards. A
  disabled table may leave `domains` out entirely, since it orders nothing.
- It is **not inherited** from `[server.acme]` to a site. Turning the deployment's orders
  off must not silence the sites, because the deployments with several unrelated hostnames
  are exactly the ones with several orders. Silence is said where it is wanted.

### The cache

An issued certificate is cached, and a **restart with a valid cached certificate serves it
immediately and spends no rate limit**. The cache is a directory beside the configuration
file (`acme-cache` by default), and the account lives there too.

That directory holds the ACME **account key** and the certificate's **private key**, so volucer
creates it `0700` and tightens any file already in it to `0600` on every start — including a
cache directory made by an older version, or by hand, which is why the mode is applied at
startup rather than only when the directory is first created. On a shared host that is the
difference between the keys being readable by every local user and by none but the service
account, and it matches what the file-backed KV driver does for the same reason. There is no
equivalent on Windows, where a file's ACL comes from its parent directory and a cache under a
user's profile is already private.

If the `cache` path exists as a *file* rather than a directory, startup fails and names the
path: every later write would fail anyway, and reporting it here is what keeps it from
surfacing later as a network error from a CA.

```bash
kill -HUP $(pidof volucer)     # pick up a certificate you replaced by hand
volucer check --config volucer.toml
```

`check` is the one to watch, because **the failure mode here is silence**. Issuance that
never succeeds looks exactly like issuance that is working: the port is up, the previous
certificate serves, and the only symptom is a certificate that expires weeks later. So
`check` prints the authority, the names, whether a certificate has ever been cached, and the
days left on each certificate being served — and shouts when the authority is production.

A deployment with no `contact` is also called out: it is the only channel the CA has to
warn that renewals are failing.

## One certificate per site

A deployment serving unrelated hostnames wants a certificate per hostname, not one
covering all of them. A site's own table gets its own order, account and certificate:

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

`domains` may be left out, and then it is **derived from the site's `hosts`** — with every
wildcard dropped, and dropped ones logged, because `*.corp.internal` is a host *pattern*
and a certificate name cannot be one. If nothing is left, that is an error naming the
wildcards, rather than an ACME request for an empty set.

`authority`, `contact` and `cache` are all optional too; omitting them inherits the
deployment's, which is what keeps several sites' certificates in one cache directory and
their orders on **one account** when the contact matches.

**`contact` is what the account is keyed by** — the account file is named after a hash of
the contact list and the directory URL — so it decides which scopes *share* an account, and
Let's Encrypt's rate limits are per account. Omitting it to inherit is therefore not just
convenience: two sites that each repeat the same contact get one account, and two that
differ get two. The second is invisible in the configuration, so the account count is worth
understanding before giving sites their own.

The site's certificate **replaces** the deployment's for that site's hosts only. A site
may also name its own `[sites.corp.tls]` pair *and* have a table here: the named pair
serves until the order replaces it, which is how a deployment migrates from a certificate
issued elsewhere.

**A name belongs to one table.** Listing it in both this and `[server.acme]` is refused,
naming both tables. It would not work: a validation handshake carries only the SNI name,
so the deployment's challenge source answers first — with a challenge certificate
committing to the *deployment's* key authorization while the CA is asking this server for
the site's — and the site's order could then never succeed.

| | where it goes |
|---|---|
| a deployment with one certificate for everything | `[server.acme]` |
| a certificate per vhost | `[sites.<name>.acme]`, inheriting the contact and cache |
| a certificate an operator manages by hand | `[sites.<name>.tls]`, or `[server.tls]` |
| settings that are staged but must not issue | `enabled = false` on either table |

## The provider

The provider is Let's Encrypt, and it is the only one that works today. `authority` is an
enum, not a URL, so production has to be named rather than typed by accident.
