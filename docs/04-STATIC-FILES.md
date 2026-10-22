# Static Files

**Serving assets, directories and precompressed files from the document root.**

## It is on by default

A site serves its own assets unless it says otherwise. You do not need to configure
anything.

Turning it on only changes the answer for a path that names a real, non-script file. A path
that names nothing, or names a `.php` script, reaches the front controller exactly as
before — so a front-controller application behaves the same either way.

The reason it is on rather than off is simply that every site has assets, and making each
one ask for its own was a line that said nothing.

## Turning it off

```toml
[server]
static_files = false
```

`serve_only = "backend"` is exactly the same state. `static_files = true` is the short form
of `enabled = true` when the rest is at its defaults.

Because static content is served over **HTTP** only, asking for it in a configuration with
no `server.http` listener is refused. Leaving the key out is not asking, so a FastCGI-only
deployment is unaffected by the default.

## A files-only site

**No backend is needed at all.** A configuration with a listener, a document root and
nothing else is valid, and `volucer serve` starts and serves it:

```toml
[server]
http = "127.0.0.1:8080"
document_root = "/srv/site"
serve_only = "static"
```

Serve it with `--front-controller ''` so a path that names no file is a 404 rather than
something handed to a script. Without that flag the default front controller (`index.php`)
is still in effect, so a path naming no file has nowhere to go and answers 500 —
`volucer check` warns about exactly this case, once, at startup.

## The rule

The document root is the same `server.document_root` that routing already resolves scripts
against, so one setting describes the whole site.

A request is answered from disk when the path names a real file under the document root,
and goes to PHP when it does not. `/assets/app.css` is a file; `/about` is not, so the
front controller still sees it. That one sentence is the whole rule.

## Two rules that always hold

These hold whether or not static serving is enabled, because both are about what PHP is
asked to run rather than about this layer:

- **A file that is not a script is never run as one.** Only `.php`, `.php3` and `.phtml`
  can be the script; anything else falls through to the front controller as application
  input. Without this, `php-cgi` would emit a file it cannot parse verbatim — `GET
  /config.json` and, worse, `GET /.env`.
- **A dotfile is never given to the application**, not even as `PATH_INFO`, for the same
  reason. It answers 404 on every method.

## What the layer serves

| Feature | Detail |
|---|---|
| Content types | A table of what a web build contains; anything unknown is `application/octet-stream` |
| Validators | `ETag` (length + mtime) and `Last-Modified`, with 304s for `If-None-Match` / `If-Modified-Since` |
| Ranges | `bytes=0-99`, `bytes=100-`, `bytes=-500`, several at once, 416 for a range past the end, `If-Range` |
| Precompressed | `file.br` then `file.gz` when the client asked — opt-in, and only when the file exists |
| Directory indexes | `index.html`, with a 301 from `/docs` to `/docs/` |
| Caching | One `Cache-Control` for everything, because a per-extension guess goes stale silently |
| Refusals | Dotfiles answer 404, and a `..` segment answers 403 rather than being clamped |
| Scripts | A `.php`/`.php3`/`.phtml` path is never served from disk, so its source cannot be disclosed |

## Methods

The static layer handles only `GET`, `HEAD` and `OPTIONS`. A `POST` to a path that names a
file goes to the front controller, so a framework can route it. Those two rules above are
what keep that from becoming a way to read a file.

## Precompressed files

```toml
[server.static_files]
precompressed = true
```

Serves `app.js.br` or `app.js.gz` when one exists and the client accepts it.

**Volucer does not produce those files** — it relays them with the right
`Content-Encoding`, which is why that path costs nothing. Generating them is your build's
job.

This is opt-in, because it assumes the build produced them, and silently serving a stale
one is worse than not serving it at all.

## Cost

Serving a file costs no PHP worker and no `spawn_blocking`: path resolution is one blocking
`stat`, and the bytes stream through in 64 KiB blocks with a content-length frame, so the
response is not chunked.

Zero-copy (`sendfile`) is deliberately not used — that would be a Unix-only second code
path — so a large file is copied per block.

## Configuration

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `true` | Serve files from the document root. |
| `precompressed` | bool | `false` | Serve `file.gz` or `file.br` when the client accepts that encoding. |
| `directory_index` | bool | `true` | Serve `index.html` for a directory. |
| `dotfiles` | string | `"deny"` | `"deny"` or `"allow"`. |
| `cache_control` | string | `"public, max-age=3600"` | The `Cache-Control` header on served files. |
| `max_multirange_bytes` | integer | `1048576` | The largest total a multi-range request may ask for. |

`dotfiles = "deny"` answers **404**, not 403. Which files exist is not something a client
needs to learn.

`dotfiles = "allow"` is only sensible when the document root holds nothing but published
assets.

**Writing the table is not the same as leaving it out.** The table states overrides, and a
table that does not say `enabled = true` does not turn the layer on — it leaves the
inherited setting alone. So this turns it *off*:

```toml
[server.static_files]
enabled = false
```

And this changes the other keys while saying nothing about whether it runs:

```toml
[server.static_files]
cache_control = "public, max-age=60"
```

A site inherits this per key, so a site naming `precompressed = true` keeps the server's
`cache_control`.

## Compression is a separate thing

Two different mechanisms, and the split matters:

| What | Compressed by | Cost per request |
|---|---|---|
| Static files | **your build**, as `.br`/`.gz` beside the file | nothing |
| Backend responses | **volucer**, if you ask for it | CPU, on the request path |

A backend's output cannot be precompressed, because it does not exist until the request
runs. That is what `[server.compression]` is for:

```toml
[server.compression]
enabled = true
encodings = ["gzip", "zstd"]   # the default; order decides ties
level = 6                      # gzip 1-9, zstd 1-19, clamped per encoding
min_length = 1024              # bodies below this are passed through
```

Off by default is a deliberate inversion of nginx's. Compressing a response spends CPU on
the request path of a server designed around one request at a time per worker slot, so
this is a choice to make knowingly rather than discover in a profile.

**gzip is first** because every client accepts it. **zstd** is offered because it is
smaller for text and current browsers support it; its window is clamped to 8 MiB, which is
the largest browsers accept.

**brotli is deliberately not offered at runtime.** Its reference implementation defaults
to quality 11, which is a build-time setting misapplied to a request path. Brotli belongs
offline, at high quality, producing the `.br` files the static layer already serves.

### What is never compressed

Whatever the configuration says:

- a body under `min_length`, or one that came out *larger* after encoding;
- a response that already carries `Content-Encoding`, or a `Content-Range` — the bytes are
  a window into another representation;
- content that is already entropy-coded: images other than SVG and icons, video, audio,
  fonts, zip/gzip/zstd archives, PDF, wasm, and `application/grpc`, which negotiates its
  own encoding.

`Vary: accept-encoding` is added to a compressed response, appended to whatever the
application already sent, so a cache cannot hand a gzip answer to a client that cannot
read it. A client sending `q=0` for both encodings gets the plain body, not a 406 — the
request is still servable.

A site can override any of it, and a list a site states **replaces** the server's rather
than adding to it:

```toml
[sites.api.compression]
enabled = true
encodings = ["zstd"]
```
