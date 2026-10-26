# Static Files

Volucer can serve static files from each site's document root, including directory
indexes and precompressed assets.

## It is on by default

Static-file serving is enabled by default for each site. No additional configuration is
required.

The static layer handles paths that resolve to real, non-script files. Paths that do not
resolve to a file, and paths that name a `.php` script, continue to the front controller
as before. Enabling static serving therefore does not change how front-controller
applications handle those requests.

## Turning it off

```toml
[server]
static_files = false
```

`serve_only = "backend"` has the same effect. When all other options use their defaults,
`static_files = true` is shorthand for `enabled = true`.

Static files are served over HTTP. Volucer rejects an explicit static-file configuration
when no `server.http` listener is configured. Omitting the setting does not enable static
serving on a FastCGI-only deployment.

## A files-only site

A files-only site does not need a backend. A listener, document root, and
`serve_only = "static"` are sufficient:

```toml
[server]
http = "127.0.0.1:8080"
document_root = "/srv/site"
serve_only = "static"
```

Use `--front-controller ''` so requests for paths that do not name a file return 404.
Without this flag, the default front controller (`index.php`) remains active. If that file
is unavailable to handle a request, Volucer returns 500. `volucer check` reports this
configuration issue at startup.

## The rule

Static files are resolved under the same document root as scripts:
`server.document_root`.

If a request path resolves to a real file under the document root, Volucer serves it from
disk. Otherwise, it dispatches the request to the application. For example,
`/assets/app.css` may resolve to a file, while `/about` can continue to the front
controller.

## Two rules that always hold

These protections apply whether or not static serving is enabled:

- **Non-script files are never executed as scripts.** Only files with `.php`, `.php3`, or
  `.phtml` extensions can be executed. Other paths are passed to the front controller as
  application input. This prevents `php-cgi` from returning the contents of files it
  cannot parse, such as `/config.json` or `/.env`.
- **Dotfiles are never passed to the application**, including as `PATH_INFO`. Requests
  for dotfiles return 404 for every method.

## What the layer serves

| Feature | Detail |
|---|---|
| Feature | Behavior |
|---|---|
| Content types | Uses a built-in table of common web content types; unknown types use `application/octet-stream`. |
| Validators | Sends `ETag` (based on length and modification time) and `Last-Modified`; supports 304 responses for `If-None-Match` and `If-Modified-Since`. |
| Ranges | Supports single and multiple byte ranges, including `bytes=0-99`, `bytes=100-`, and `bytes=-500`; returns 416 for ranges beyond the file and supports `If-Range`. |
| Precompressed files | Tries `file.br`, then `file.gz`, when requested by the client. This is opt-in and requires the file to exist. |
| Directory indexes | Serves `index.html` for directories and redirects `/docs` to `/docs/` with a 301 response. |
| Caching | Applies one `Cache-Control` value to all served files. |
| Refusals | Returns 404 for dotfiles and 403 for paths containing a `..` segment. |
| Scripts | Never serves `.php`, `.php3`, or `.phtml` files from disk, preventing source disclosure. |

## Methods

The static layer handles `GET`, `HEAD`, and `OPTIONS`. Other methods, including `POST`,
are dispatched to the front controller even when the path names a file. This allows the
application to route those requests without exposing the file contents.

## Precompressed files

```toml
[server.static_files]
precompressed = true
```

When the client accepts a supported encoding, serves `app.js.br` or `app.js.gz` if the
corresponding file exists.

Volucer does not generate compressed files. It serves the prebuilt file and sets the
appropriate `Content-Encoding` header. Generate these files as part of your build.

This option is disabled by default because it assumes your build produces the compressed
files. A missing file is not an error; Volucer serves the original instead.

## Cost

Serving a file does not use a PHP worker or `spawn_blocking`. Path resolution performs one
blocking `stat`; file contents are streamed in 64 KiB blocks with a content-length frame,
so the response is not chunked.

Volucer does not use zero-copy `sendfile`; that would require a separate Unix-only code
path. File data is copied in blocks as it is streamed.

## Configuration

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `true` | Serve files from the document root. |
| `precompressed` | bool | `false` | Serve `file.br` or `file.gz` when the client accepts that encoding. |
| `directory_index` | bool | `true` | Serve `index.html` for a directory. |
| `dotfiles` | string | `"deny"` | Set to `"deny"` or `"allow"` to control dotfile access. |
| `cache_control` | string | `"public, max-age=3600"` | Set the `Cache-Control` response header for served files. |
| `max_multirange_bytes` | integer | `1048576` | Limit the total size requested by a multi-range request. |

With `dotfiles = "deny"`, requests return 404 rather than 403, so the response does not
reveal whether a dotfile exists.

Use `dotfiles = "allow"` only when the document root contains published assets and no
sensitive dotfiles.

The table specifies overrides to the current settings. Omitting `enabled` does not
automatically enable or disable static serving; it leaves the inherited value unchanged.
For example, this disables the static layer:

```toml
[server.static_files]
enabled = false
```

This changes `cache_control` while leaving `enabled` unchanged:

```toml
[server.static_files]
cache_control = "public, max-age=60"
```

Site settings are inherited per key. A site that sets `precompressed = true`, for
example, continues to inherit the server's `cache_control`.

## Compression is a separate thing

Static-file compression and response compression are separate mechanisms:

| What | Compressed by | Cost per request |
|---|---|---|
| Static files | Your build, producing `.br` or `.gz` files beside the originals | No compression work per request |
| Backend responses | Volucer, when enabled | CPU on the request path |

Backend response bodies are generated for each request, so they cannot be precompressed
in the build. Configure runtime response compression with `[server.compression]`:

```toml
[server.compression]
enabled = true
encodings = ["gzip", "zstd"]   # the default; order decides ties
level = 6                      # gzip 1-9, zstd 1-19, clamped per encoding
min_length = 1024              # bodies below this are passed through
```

Runtime compression is disabled by default. It uses CPU while handling a request, so
enable it when the reduction in response size is worth that cost for your workload.

Gzip is listed first because it is widely supported. Zstandard is also available and can
produce smaller text responses; its window is limited to 8 MiB, the largest size accepted
by current browsers.

Brotli is not offered for runtime compression. Its reference implementation defaults to
quality 11, which is more appropriate for build-time processing. Generate Brotli
`.br` files ahead of time and serve them with the static-file layer instead.

### What is never compressed

These responses are not compressed, regardless of configuration:

- Bodies smaller than `min_length`, or bodies that would become larger after compression.
- Responses that already have `Content-Encoding` or `Content-Range`. A range response is
  a portion of another representation.
- Content that is already compressed or encoded: images other than SVG and icons, video,
  audio, fonts, ZIP/GZIP/Zstandard archives, PDF, WebAssembly, and `application/grpc`,
  which negotiates its own encoding.

Compressed responses include `Vary: accept-encoding`, appended to any existing `Vary`
value. This prevents caches from serving a compressed response to a client that does not
accept that encoding. If a client sets `q=0` for both supported encodings, Volucer sends
the uncompressed response rather than returning 406.

A site can override these settings. If it specifies an `encodings` list, that list
replaces the server-level list rather than adding to it:

```toml
[sites.api.compression]
enabled = true
encodings = ["zstd"]
```
