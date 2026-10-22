# Volucer Documentation

Volucer is a cross-platform PHP application server. One binary, two ways to run PHP
applications, and no embedded PHP runtime — `php`, `php-cgi` and `php-fpm` are started as
ordinary external processes.

This guide is for developers who already have a PHP application and want to run it on
volucer. If you are looking for the reasoning behind the project, that lives in the
[design journals](../internal/README.md) rather than here.

## Start here

- **[Getting Started](00-GETTING-STARTED.md)** — Install the binary, write a
  `volucer.toml`, serve your first request, and move off `php-fpm`.

## Running your application

- **[Backends](02-BACKENDS.md)** — The five ways volucer can run PHP: a persistent
  worker, an attached worker pool, an existing `php-fpm`, a spawned `php-cgi` pool, or
  an HTTP origin. Which one to pick, and why the choice matters.
- **[Routing and Sites](03-ROUTING-AND-SITES.md)** — Send different paths to different
  backends, and serve several applications from one process.
- **[Static Files](04-STATIC-FILES.md)** — Serve assets, directories and precompressed
  files from the document root.
- **[TLS and ACME](05-TLS-AND-ACME.md)** — HTTPS with your own certificate, or
  automatically issued ones.

## Configuration

- **[Configuration](01-CONFIGURATION.md)** — Every key the configuration file accepts,
  with its default. This is the reference; the other pages explain what the keys do.

## Writing a worker application

- **[Worker Applications](07-WORKER-APPLICATIONS.md)** — Write a PHP worker: keep the
  application resident, handle requests, and clear your own request scope.
- **[Services](08-SERVICES.md)** — Key-value storage, locks, queues and gRPC, from
  inside a running request.
- **[Worker Protocol](09-WORKER-PROTOCOL.md)** — The wire protocol, for anyone
  implementing a worker in another language.

## Operating volucer

- **[Observability](06-OBSERVABILITY.md)** — Access logs, request IDs, rate limiting,
  metrics and distributed tracing.
- **[Operations](11-OPERATIONS.md)** — Reloading, running behind nginx, containers,
  systemd and the release process.
- **[SDKs and Frameworks](10-SDKS-AND-FRAMEWORKS.md)** — The PHP, Python and Ruby SDKs,
  and the framework integrations built on them.
- **[Benchmarks](12-BENCHMARKS.md)** — Measured performance against RoadRunner,
  FrankenPHP and `nginx` + `php-fpm`, at 16 and 32 PHP processes, and what each
  of them costs in memory.
## Reference

- **[The worker protocol](https://github.com/sailantis/volucer/blob/main/docs/worker-protocol.md)**
  — The normative specification, pinned byte-for-byte by every SDK.
- **[volucer.example.toml](https://github.com/sailantis/volucer/blob/main/volucer.example.toml)**
  — An annotated configuration file covering every section.
