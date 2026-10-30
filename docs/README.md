# Volucer Documentation

Volucer is a cross-platform application server for PHP, Python and Ruby. It does not embed a
runtime: for PHP it starts `php` or `php-cgi` as external processes, or connects to an
existing `php-fpm` process; for Python and Ruby it speaks the worker protocol over the
child's standard streams; and it runs **any FastCGI server** — `php-fpm`, `gunicorn`, `flup`,
a Rack adapter — because FastCGI is a protocol rather than a PHP feature.

This guide explains how to configure, run, and maintain Volucer.

## Start here

- **[Getting Started](00-GETTING-STARTED.md)** — Install the binary, write a
  `volucer.toml`, serve your first request, and move off `php-fpm`.

## Running your application

- **[Backends](02-BACKENDS.md)** — The five ways Volucer can run an application: a persistent
  worker, an attached worker pool, an existing FastCGI server, a spawned FastCGI pool, or
  an HTTP origin. Which one to pick, and why the choice matters.
- **[Routing and Sites](03-ROUTING-AND-SITES.md)** — Send different paths to different
  backends, and serve several applications from one process.
- **[Static Files](04-STATIC-FILES.md)** — Serve assets, directories and precompressed
  files from the document root.
- **[TLS and ACME](05-TLS-AND-ACME.md)** — HTTPS with your own certificate, or
  automatically issued ones.

## Configuration

- **[Configuration](01-CONFIGURATION.md)** — Configuration keys and their defaults.
  with its default. This is the reference; the other pages explain what the keys do.

## Writing a worker application

- **[Worker Applications](07-WORKER-APPLICATIONS.md)** — Write a PHP worker: keep the
  application resident, handle requests, and clear your own request scope.
- **[Services](08-SERVICES.md)** — Key-value storage, locks, queues and gRPC, from
  inside a running request.
- **[Worker Protocol](09-WORKER-PROTOCOL.md)** — A summary of the wire protocol.
  implementing a worker in another language.

## Operating volucer

- **[Observability](06-OBSERVABILITY.md)** — Access logs, request IDs, rate limiting,
  metrics and distributed tracing.
- **[Operations](11-OPERATIONS.md)** — Reloading, running behind nginx, containers,
  systemd and the release process.
- **[Deploying](14-DEPLOYING.md)** — Shipping application code without dropping a request:
  the roll, why a sweep may not finish in one pass, and the loop a deploy script needs.
- **[SDKs and Frameworks](10-SDKS-AND-FRAMEWORKS.md)** — The PHP, Python and Ruby SDKs,
  and the framework integrations built on them.
- **[Benchmarks](12-BENCHMARKS.md)** — Performance and memory measurements against
  RoadRunner, FrankenPHP, and `nginx` + `php-fpm`, including the test method and results.
- **[Carrying an upstream patch](13-CARRYING-AN-UPSTREAM-PATCH.md)** — Why a dependency may be
  pinned to a fork, and what that does and does not protect.

## Reference

- **[The worker protocol](https://github.com/sailantis/volucer/blob/main/docs/worker-protocol.md)**
  — The normative protocol specification.
- **[volucer.example.toml](https://github.com/sailantis/volucer/blob/main/volucer.example.toml)**
  — An annotated configuration file covering every section.
