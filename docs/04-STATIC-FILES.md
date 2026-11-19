# Static files

Volucer serves static files from each site's document root, including directory indexes
and precompressed assets.

## Enabled by default

Static-file serving is enabled by default for every site. No configuration is required.

The static layer serves paths that resolve to real, non-script files. Requests that do not
resolve to a file, or that name a PHP script, continue to the front controller. Enabling
static serving does not change how front-controller applications handle those requests.

## Disable static serving

```toml
[server]
static_files = false
```

`serve_only = "backend"` also disables static serving. When other options use their
defaults, `static_files = true` is shorthand for `enabled = true`.

Static files are served over HTTP. Volucer rejects an explicit static-file setting if no
`server.http` listener is configured. On FastCGI-only deployments, omitting the setting
leaves static serving disabled.

## Files-only site

A files-only site needs no backend. Configure a listener, document root, and
`serve_only = "static"`:

```toml
[server]
http = "127.0.0.1:8080"
document_root = "/srv/site"
serve_only = "static"
```

Use `--front-controller ''` to return 404 for paths that do not name a file. Otherwise,
the default front controller (`index.php`) remains active. If it cannot handle a request,
Volucer returns 500. `volucer check` reports this configuration issue at startup.

A `404` this layer produces is answered with a body you can control:

```toml
[server.error_pages]
"404" = "pages/404.html"
```

The file is read from the configuration file's directory, not the document root, and its
extension decides the `Content-Type` — so `pages/404.json` answers a JSON API. Without a
configured page the refusal is a status with `Cache-Control: no-store` and no body. See
[Error pages](06-OBSERVABILITY.md#error-pages).

## Request handling

By default, static files and scripts use the same document root:
`server.document_root`.

If a request path resolves to a real file under the document root, Volucer serves it from
disk. Otherwise, it dispatches the request to the application. For example,
`/assets/app.css` may resolve to a file, while `/about` may continue to the front controller.

## Mount a directory with a route

A `[[server.routes]]` entry with `root` serves its subtree from that directory. The
route's `path_prefix` is the mount point and is **stripped** before joining the request
path. The route root replaces the document root rather than nesting inside it:

```toml
[[server.routes]]
path_prefix = "/downloads"
root = "/srv/files"
policy = "static"
```

Volucer serves `/downloads/report.pdf` from `/srv/files/report.pdf`. Range requests,
`ETag`, `If-Range`, and precompressed-variant handling work as usual. Use this to serve
a directory outside the document root without a symlink or application script.

The route root does not change:

- **Script resolution.** Files ending in `.php`, `.php3`, or `.phtml` are never served
  from disk, including under a route root. The site's script-extension guard still applies,
  so a mount cannot expose source files.
- **Path protections.** Volucer rejects `..` before accessing the filesystem and hides
  dotfiles, as it does under the document root.

Set `root` to an absolute path that exists; otherwise startup fails. A route root also
requires `path_prefix`, which defines its mount point.

## Security rules

These protections apply whether or not static serving is enabled:

- **Non-script files are never executed as scripts.** Only files with `.php`, `.php3`, or
  `.phtml` extensions can be executed. Other paths are passed to the front controller as
  application input. This prevents `php-cgi` from returning files it cannot parse, such as
  `/config.json` or `/.env`.
- **Dotfiles are never passed to the application**, including as `PATH_INFO`. Requests
  for dotfiles return 404 for every method.

## Supported features

| Feature | Detail |
|---|---|
| Content types | Uses a built-in table of common web content types. Unknown types use `application/octet-stream`. |
| Validators | Sends `ETag` (based on file length and modification time) and `Last-Modified`; supports 304 responses for `If-None-Match` and `If-Modified-Since`. |
| Ranges | Supports single and multiple byte ranges, such as `bytes=0-99`, `bytes=100-`, and `bytes=-500`. Returns 416 for ranges beyond the file and supports `If-Range`. |
| Precompressed files | Tries `file.br`, then `file.gz`, when requested by the client. Disabled by default; the requested file must exist. |
| Directory indexes | Serves `index.html` for directories and redirects `/docs` to `/docs/` with a 301 response. |
| Caching | Applies the configured `Cache-Control` value to all served files. |
| Refusals | Returns 404 for dotfiles and 403 for paths containing a `..` segment. Both carry `Cache-Control: no-store`, so a refusal is never heuristically cached. |
| Scripts | Never serves `.php`, `.php3`, or `.phtml` files from disk, preventing source disclosure. |

## Methods

The static layer handles `GET`, `HEAD`, and `OPTIONS`. Other methods, including `POST`,
go to the front controller even when the path names a file. The application can handle
those requests without exposing the file contents.

## Precompressed files

```toml
[server.static_files]
precompressed = true
```

If the client accepts a supported encoding, Volucer serves `app.js.br` or `app.js.gz` when
the corresponding file exists.

Volucer does not generate compressed files. It serves prebuilt files and sets the
appropriate `Content-Encoding` header. Generate these files during your build.

This option is disabled by default because it assumes the build produces compressed files.
If a compressed file is missing, Volucer serves the original instead.

## Performance

Static-file requests do not use a PHP worker. File lookup runs on the runtime thread: one
`stat` on Unix, and `open` followed by `fstat` on Windows for non-304 responses.

### Windows file lookup

Windows uses `open` followed by `fstat`. For existing files, this scales better than
`stat` (1.44x versus 1.10x throughput at 12 threads on a 12-core host) and uses one handle
instead of two. Interleaved `GET /index.html` results at c=100:

```text
                     stat + open      open + fstat
  200, one URL          10,107          14,550      +44%
  200, twelve URLs      22,177          27,583      +24%
  304 (no body)         19,737          19,737      unchanged
```

`304` responses still use only `stat`, so Volucer does not open the file. Unix keeps `stat`
first: `statx` takes about 1.3 µs and scales 7.6x, so changing the lookup order is within
measurement noise in interleaved tests from c=12 to c=400. Opening first could also block
on a FIFO under the document root.

On the same hardware, static-file performance was:

```text
                     Windows     Linux (ext4)
  static files, c=100    10,470       82,845
  static files, c=400    10,072      158,718
  metadata, one thread    34.3 us      1.3 us
```

Windows handles 6–8x fewer requests per second, largely because of filesystem costs. A Go
server using the same `os.Stat` and `os.ReadFile` calls reached 8,546 rps, compared with
59,721 rps for a route with no file access. Windows is supported, but performance tuning
has focused on Linux.

On Unix, a response slice of 256 KiB or less is read with one `pread` on the runtime
thread. On Windows, Volucer seeks and reads the file on that thread. The slice is sent in
one in-memory frame with an exact `Content-Length`; no stream needs to be polled. Larger
slices are streamed from a `tokio` file in 512 KiB blocks with a `Content-Length` frame,
so the response is not chunked.

Both sizes are benchmark-driven. Reading a small file through `tokio::fs::File` requires a
`spawn_blocking` call, two thread wakeups, and a `Mutex` handoff. Removing that overhead
improved throughput 3.1x for a 4,976-byte file. Streaming in 64 KiB blocks requires 16
reads and wakeups per MiB; 512 KiB blocks improved throughput 3.6x for a 1 MiB file.
1 MiB blocks were also tested and were slightly slower.

Serving a small file blocks a runtime thread only for the `pread` from the page cache,
which takes microseconds on a local filesystem. On a network filesystem, each `stat` or
read requires a round trip. Keep the document root local and serve remote files through
the application instead.

Volucer does not use zero-copy `sendfile`, which would require a separate Unix-only code
path. It is the one measured case where another server is faster: at 4,976 bytes, a
`sendfile`-based server reached about 111,000 responses per second, compared with 243,000
here. From 100 KiB upward, copy-based streaming was faster. File data is copied as it is
served.

## Configuration

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `true` | Serve files from the document root. |
| `precompressed` | bool | `false` | Serve `file.br` or `file.gz` when the client accepts that encoding. |
| `directory_index` | bool | `true` | Serve `index.html` for a directory. |
| `dotfiles` | string | `"deny"` | Set to `"deny"` or `"allow"` to control dotfile access. |
| `cache_control` | string | `"public, max-age=3600"` | Set the `Cache-Control` response header for served files. |
| `max_multirange_bytes` | integer | `1048576` | Limit the total size requested by a multi-range request. |

With `dotfiles = "deny"`, requests return 404 rather than 403. This does not reveal
whether a dotfile exists.

Use `dotfiles = "allow"` only when the document root contains published assets and no
sensitive dotfiles.

The table lists settings that can override inherited values. Omitting `enabled` preserves
the inherited value. For example, this disables the static layer:

```toml
[server.static_files]
enabled = false
```

This changes `cache_control` while leaving `enabled` unchanged:

```toml
[server.static_files]
cache_control = "public, max-age=60"
```

Site settings inherit each key independently. For example, a site that sets
`precompressed = true` still inherits the server's `cache_control`.

## Static-file and response compression

Static-file compression and response compression are separate:

| Type | Compressed by | Per-request cost |
|---|---|---|
| Static files | Your build, which creates `.br` or `.gz` files beside the originals | No compression work |
| Backend responses | Volucer, when enabled | CPU on the request path |

Backend response bodies are generated for each request, so they cannot be precompressed
during the build. Configure runtime compression with `[server.compression]`:

```toml
[server.compression]
enabled = true
encodings = ["gzip", "zstd"]   # the default; order decides ties
level = 6                      # gzip 1-9, zstd 1-19, clamped per encoding
min_length = 1024              # bodies below this are passed through
```

Runtime compression is disabled by default and uses CPU on the request path. Enable it
when the reduction in response size is worth the cost for your workload.

Gzip is listed first because it is widely supported. Zstandard can also produce smaller
text responses. Its window is limited to 8 MiB, the largest size accepted by current
browsers.

Brotli is not available for runtime compression. Its reference implementation defaults
to quality 11, which is better suited to build-time processing. Generate `.br` files in
advance and serve them with the static-file layer.

### Responses excluded from compression

These responses are not compressed, regardless of configuration:

- Bodies smaller than `min_length`, or bodies that would become larger after compression.
- Responses that already have `Content-Encoding` or `Content-Range`. A range response
  represents only part of the original content.
- Content that is already compressed or encoded: images other than SVG and icons, video,
  audio, fonts, ZIP/GZIP/Zstandard archives, PDF, WebAssembly, and `application/grpc`,
  which negotiates its own encoding.

Compressed responses include `Vary: accept-encoding`, appended to any existing `Vary`
value. This prevents caches from serving a compressed response to a client that does not
accept that encoding. If a client sets `q=0` for both supported encodings, Volucer sends
the uncompressed response rather than returning 406.

Sites can override these settings. A site's `encodings` list replaces the server-level
list rather than extending it:

```toml
[sites.api.compression]
enabled = true
encodings = ["zstd"]
```
