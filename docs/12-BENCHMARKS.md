# Benchmarks

A comparison of Volucer, RoadRunner, FrankenPHP, and nginx with PHP-FPM using a shared benchmark harness. Results include throughput, latency, and process-tree memory.

All values come from committed benchmark results in
[`volucer-competition`](https://github.com/sailantis/volucer-competition). Tables and
charts are generated from the same measurements. Missing successful measurements are
shown as `-`, not as zero. The repository includes the method, raw rows, and controls.

Measurements were interleaved on the same host using the same harness and PHP build.
Each chart identifies its host, method, and measured software versions.

## C1 — Echo — `GET /echo`

Measurements were interleaved across server arms, process counts, and load levels on the same host. The order rotated between rounds to reduce ordering effects. **PHP procs** is the number of PHP processes serving requests and provides a resource-use context for each result.

With `spawn_cgi` and `children`, multiple PHP processes serve through one slot. The process-count column makes this configuration comparable with other arms that use one process per slot.

Each chart shows request rate in requests per second, with bars that start at zero for direct visual comparison.

The label on each bar is its throughput factor relative to the fastest server in that chart. The dashed line marks that reference. For example, `x2.5` means the server handled about 1/2.5 of the reference's requests per second. The reference can differ between charts.

The range shows the minimum and maximum of the round-paired throughput ratios, converted back to requests per second. Pairing measurements from the same round helps account for host variation during the run; the range describes the comparison rather than either server alone.

If a factor range includes 1.0, the measured range includes equal throughput. The table marks that result **tie** to distinguish it from a comparison whose observed range stays above or below 1.0.

### OPcache on

![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-echo-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-echo-on-c64.svg)
![OPcache on at c=96](images/benchmarks/benchmarks/benchmarks-echo-on-c96.svg)

The **factor** compares each row with the fastest server in the table (`x1.0`). A factor of `x2.5` means about 40% of the reference throughput. The range shows the round-paired comparison; **tie** marks a range that includes 1.0.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 32 | **110,830** | 0.82 ms | 2.02 ms | x1.0 |
| volucer | attach (warm, unix relay) | 16 | **85,807** | 1.08 ms | 3.25 ms | x1.3 (1.29–1.31) |
| RoadRunner | worker (warm, pipe) | 32 | **74,833** | 1.14 ms | 3.61 ms | x1.5 (1.48–1.50) |
| RoadRunner | worker (warm, pipe) | 16 | **69,785** | 1.28 ms | 2.73 ms | x1.6 (1.59–1.61) |
| FrankenPHP | worker (warm, embedded) | 16 | **51,514** | 1.75 ms | 5.33 ms | x2.2 (2.13–2.20) |
| FrankenPHP | worker (warm, embedded) | 32 | **45,004** | 1.9 ms | 5.85 ms | x2.5 (2.40–3.68) |
| nginx + php-fpm | cold, unix socket | 32 | **41,428** | 2.22 ms | 4.13 ms | x2.7 (2.66–2.73) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **40,096** | 2.32 ms | 4.86 ms | x2.8 (2.74–2.83) |
| nginx + php-fpm | cold, unix socket | 16 | **38,440** | 2.46 ms | 3.25 ms | x2.9 (2.88–2.92) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **34,090** | 2.79 ms | 7.22 ms | x3.3 (3.20–3.33) |

This table compares the two cold PHP process pools at equal process counts: `spawn_cgi` with `children` and nginx with php-fpm.

| PHP procs | server | model | rps | spawn_cgi ÷ php-fpm |
|---:|---|---|---:|---:|
| 16 | volucer | spawn_cgi (cold, unix, children) | **33,364** | x0.87 |
| 16 | nginx + php-fpm | cold, unix socket | **38,435** | — |
| 32 | volucer | spawn_cgi (cold, unix, children) | **38,895** | x0.93 |
| 32 | nginx + php-fpm | cold, unix socket | **41,869** | — |

The factor is `spawn_cgi` throughput divided by php-fpm throughput at the same process count. Values above 1.0 indicate higher measured throughput for `spawn_cgi`; values below 1.0 indicate higher throughput for php-fpm.

This table compares each arm at the smallest and largest published process counts. Ratios are paired by measurement round. A ratio above 1.0 indicates higher throughput at the larger process count; a range including 1.0 means the measured rounds also include equal throughput.

| server | model | 16 procs | 32 procs | 32/16 (range) |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 85,835 | 108,569 | x1.26 (1.24–1.29) |
| RoadRunner | worker (warm, pipe) | 69,453 | 74,179 | x1.07 (1.06–1.07) |
| FrankenPHP | worker (warm, embedded) | 49,079 | 42,937 | x0.87 (0.85–0.94) |
| volucer | spawn_cgi (cold, unix, children) | 33,364 | 38,895 | x1.17 (1.16–1.17) |
| nginx + php-fpm | cold, unix socket | 38,435 | 41,869 | x1.09 (1.07–1.11) |

The 64-process tier was measured but is not included in the throughput comparison. Its memory measurements are included below.

## What it costs in memory

Memory is measured after the run across the full process tree. The table reports **proportional set size (PSS)** in MiB. PSS apportions each shared memory page among the processes that use it, making it useful for comparing deployments with different process models.

RSS is shown in parentheses for reference. Summed across processes, RSS counts shared pages once for every process that maps them, while PSS divides their contribution among those processes.

![Proportional memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory-echo.svg)

| server | model | 16 procs | 32 procs | 64 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 82.0 (487) | 100.2 (878) | - |
| RoadRunner | worker (warm, pipe) | 420.5 (950) | 749.6 (1,837) | - |
| FrankenPHP | worker (warm, embedded) | 146.4 (146) | 170.0 (170) | - |
| volucer | spawn_cgi (cold, unix, children) | 73.3 (523) | 93.2 (973) | - |
| nginx + php-fpm | cold, unix socket | 63.6 (543) | 80.5 (1,003) | - |

Each value is PSS in MiB; RSS in MiB is shown in parentheses.

### The same PHP processes, arranged two ways

This comparison holds the number of serving PHP processes constant: one process per slot versus multiple forked children behind a slot.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes-echo.svg)

| shape | PHP processes | median PSS (MiB) | min | max | vs one per slot |
|---|---:|---:|---:|---:|---:|
| 8 separate slots | 8 | **193.8** | 193.6 | 194.6 | — |
| 1 slot × 8 children | 8 | **65.5** | 64.7 | 66.2 | 3.0× less |
| 16 separate slots | 16 | **340.3** | 339.9 | 340.9 | — |
| 1 slot × 16 children | 16 | **73.3** | 72.8 | 73.8 | 4.6× less |
| 32 separate slots | 32 | **633.7** | 633.0 | 634.3 | — |
| 1 slot × 32 children | 32 | **93.2** | 92.5 | 94.2 | 6.8× less |

The **vs one per slot** value is the separate-slot median divided by the forked-pool median at the same process count. For example, `4.5× less` means the forked pool's median PSS is about one quarter of the separate-slot median.

Forked children can share memory pages inherited from their master. PSS accounts for this sharing, which is why the forked-pool and separate-slot arrangements can have different total PSS at the same number of serving processes.

## C3 — API Items — `GET /api/items`

Measurements were interleaved across server arms, process counts, and load levels on the same host. The order rotated between rounds to reduce ordering effects. **PHP procs** is the number of PHP processes serving requests and provides a resource-use context for each result.

With `spawn_cgi` and `children`, multiple PHP processes serve through one slot. The process-count column makes this configuration comparable with other arms that use one process per slot.

Each chart shows request rate in requests per second, with bars that start at zero for direct visual comparison.

The label on each bar is its throughput factor relative to the fastest server in that chart. The dashed line marks that reference. For example, `x2.5` means the server handled about 1/2.5 of the reference's requests per second. The reference can differ between charts.

The range shows the minimum and maximum of the round-paired throughput ratios, converted back to requests per second. Pairing measurements from the same round helps account for host variation during the run; the range describes the comparison rather than either server alone.

If a factor range includes 1.0, the measured range includes equal throughput. The table marks that result **tie** to distinguish it from a comparison whose observed range stays above or below 1.0.

### OPcache on

![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-api-items-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-api-items-on-c64.svg)
![OPcache on at c=96](images/benchmarks/benchmarks/benchmarks-api-items-on-c96.svg)

The **factor** compares each row with the fastest server in the table (`x1.0`). A factor of `x2.5` means about 40% of the reference throughput. The range shows the round-paired comparison; **tie** marks a range that includes 1.0.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 32 | **72,286** | 1.25 ms | 3.14 ms | x1.0 |
| volucer | attach (warm, unix relay) | 16 | **55,913** | 1.68 ms | 4.8 ms | x1.3 (1.26–1.31) |
| RoadRunner | worker (warm, pipe) | 16 | **45,756** | 1.99 ms | 3.72 ms | x1.6 (1.55–1.60) |
| RoadRunner | worker (warm, pipe) | 32 | **45,394** | 1.97 ms | 4.68 ms | x1.6 (1.55–1.60) |
| FrankenPHP | worker (warm, embedded) | 16 | **41,469** | 2.2 ms | 6.42 ms | x1.7 (1.70–1.77) |
| FrankenPHP | worker (warm, embedded) | 32 | **34,846** | 2.5 ms | 7.24 ms | x2.1 (2.00–2.14) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **15,388** | 6.15 ms | 12.31 ms | x4.7 (4.59–4.86) |
| nginx + php-fpm | cold, unix socket | 32 | **14,876** | 6.31 ms | 8.89 ms | x4.9 (4.77–4.91) |
| nginx + php-fpm | cold, unix socket | 16 | **12,916** | 7.33 ms | 9.26 ms | x5.6 (5.40–5.69) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **12,757** | 7.46 ms | 19.6 ms | x5.7 (5.47–5.78) |

This table compares the two cold PHP process pools at equal process counts: `spawn_cgi` with `children` and nginx with php-fpm.

| PHP procs | server | model | rps | spawn_cgi ÷ php-fpm |
|---:|---|---|---:|---:|
| 16 | volucer | spawn_cgi (cold, unix, children) | **12,802** | x0.98 |
| 16 | nginx + php-fpm | cold, unix socket | **12,997** | — |
| 32 | volucer | spawn_cgi (cold, unix, children) | **15,448** | x1.04 |
| 32 | nginx + php-fpm | cold, unix socket | **14,840** | — |

The factor is `spawn_cgi` throughput divided by php-fpm throughput at the same process count. Values above 1.0 indicate higher measured throughput for `spawn_cgi`; values below 1.0 indicate higher throughput for php-fpm.

This table compares each arm at the smallest and largest published process counts. Ratios are paired by measurement round. A ratio above 1.0 indicates higher throughput at the larger process count; a range including 1.0 means the measured rounds also include equal throughput.

| server | model | 16 procs | 32 procs | 32/16 (range) |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 55,974 | 68,609 | x1.23 (1.20–1.25) |
| RoadRunner | worker (warm, pipe) | 45,774 | 45,479 | x0.99 (0.98–1.00) — tie |
| FrankenPHP | worker (warm, embedded) | 39,833 | 32,362 | x0.81 (0.80–0.82) |
| volucer | spawn_cgi (cold, unix, children) | 12,802 | 15,448 | x1.21 (1.19–1.23) |
| nginx + php-fpm | cold, unix socket | 12,997 | 14,840 | x1.14 (1.14–1.18) |

The 64-process tier was measured but is not included in the throughput comparison. Its memory measurements are included below.

## What it costs in memory

Memory is measured after the run across the full process tree. The table reports **proportional set size (PSS)** in MiB. PSS apportions each shared memory page among the processes that use it, making it useful for comparing deployments with different process models.

RSS is shown in parentheses for reference. Summed across processes, RSS counts shared pages once for every process that maps them, while PSS divides their contribution among those processes.

![Proportional memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory-api-items.svg)

| server | model | 16 procs | 32 procs | 64 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 82.0 (487) | 100.2 (878) | - |
| RoadRunner | worker (warm, pipe) | 420.5 (950) | 749.6 (1,837) | - |
| FrankenPHP | worker (warm, embedded) | 146.4 (146) | 170.0 (170) | - |
| volucer | spawn_cgi (cold, unix, children) | 73.3 (523) | 93.2 (973) | - |
| nginx + php-fpm | cold, unix socket | 63.6 (543) | 80.5 (1,003) | - |

Each value is PSS in MiB; RSS in MiB is shown in parentheses.

### The same PHP processes, arranged two ways

This comparison holds the number of serving PHP processes constant: one process per slot versus multiple forked children behind a slot.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes-api-items.svg)

| shape | PHP processes | median PSS (MiB) | min | max | vs one per slot |
|---|---:|---:|---:|---:|---:|
| 8 separate slots | 8 | **193.8** | 193.6 | 194.6 | — |
| 1 slot × 8 children | 8 | **65.5** | 64.7 | 66.2 | 3.0× less |
| 16 separate slots | 16 | **340.3** | 339.9 | 340.9 | — |
| 1 slot × 16 children | 16 | **73.3** | 72.8 | 73.8 | 4.6× less |
| 32 separate slots | 32 | **633.7** | 633.0 | 634.3 | — |
| 1 slot × 32 children | 32 | **93.2** | 92.5 | 94.2 | 6.8× less |

The **vs one per slot** value is the separate-slot median divided by the forked-pool median at the same process count. For example, `4.5× less` means the forked pool's median PSS is about one quarter of the separate-slot median.

Forked children can share memory pages inherited from their master. PSS accounts for this sharing, which is why the forked-pool and separate-slot arrangements can have different total PSS at the same number of serving processes.

## C5 — HTML Items — `GET /items-fast`

Measurements were interleaved across server arms, process counts, and load levels on the same host. The order rotated between rounds to reduce ordering effects. **PHP procs** is the number of PHP processes serving requests and provides a resource-use context for each result.

With `spawn_cgi` and `children`, multiple PHP processes serve through one slot. The process-count column makes this configuration comparable with other arms that use one process per slot.

Each chart shows request rate in requests per second, with bars that start at zero for direct visual comparison.

The label on each bar is its throughput factor relative to the fastest server in that chart. The dashed line marks that reference. For example, `x2.5` means the server handled about 1/2.5 of the reference's requests per second. The reference can differ between charts.

The range shows the minimum and maximum of the round-paired throughput ratios, converted back to requests per second. Pairing measurements from the same round helps account for host variation during the run; the range describes the comparison rather than either server alone.

If a factor range includes 1.0, the measured range includes equal throughput. The table marks that result **tie** to distinguish it from a comparison whose observed range stays above or below 1.0.

### OPcache on

![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-items-fast-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-items-fast-on-c64.svg)
![OPcache on at c=96](images/benchmarks/benchmarks/benchmarks-items-fast-on-c96.svg)

The **factor** compares each row with the fastest server in the table (`x1.0`). A factor of `x2.5` means about 40% of the reference throughput. The range shows the round-paired comparison; **tie** marks a range that includes 1.0.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 32 | **51,281** | 1.79 ms | 4.16 ms | x1.0 |
| volucer | attach (warm, unix relay) | 16 | **41,433** | 2.28 ms | 6.24 ms | x1.2 (1.20–1.25) |
| RoadRunner | worker (warm, pipe) | 32 | **31,707** | 2.85 ms | 6.45 ms | x1.6 (1.58–1.62) |
| RoadRunner | worker (warm, pipe) | 16 | **31,342** | 2.92 ms | 5.08 ms | x1.6 (1.60–1.64) |
| FrankenPHP | worker (warm, embedded) | 16 | **19,341** | 4.72 ms | 8.56 ms | x2.7 (2.59–2.69) |
| FrankenPHP | worker (warm, embedded) | 32 | **18,164** | 4.85 ms | 13.14 ms | x2.8 (2.72–2.86) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **12,631** | 7.49 ms | 14.8 ms | x4.1 (3.90–4.11) |
| nginx + php-fpm | cold, unix socket | 32 | **12,165** | 7.72 ms | 10.71 ms | x4.2 (4.06–4.24) |
| nginx + php-fpm | cold, unix socket | 16 | **10,786** | 8.82 ms | 10.8 ms | x4.8 (4.59–4.77) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **10,571** | 8.98 ms | 23.65 ms | x4.9 (4.65–4.88) |

This table compares the two cold PHP process pools at equal process counts: `spawn_cgi` with `children` and nginx with php-fpm.

| PHP procs | server | model | rps | spawn_cgi ÷ php-fpm |
|---:|---|---|---:|---:|
| 16 | volucer | spawn_cgi (cold, unix, children) | **10,718** | x0.99 |
| 16 | nginx + php-fpm | cold, unix socket | **10,782** | — |
| 32 | volucer | spawn_cgi (cold, unix, children) | **12,607** | x1.02 |
| 32 | nginx + php-fpm | cold, unix socket | **12,318** | — |

The factor is `spawn_cgi` throughput divided by php-fpm throughput at the same process count. Values above 1.0 indicate higher measured throughput for `spawn_cgi`; values below 1.0 indicate higher throughput for php-fpm.

This table compares each arm at the smallest and largest published process counts. Ratios are paired by measurement round. A ratio above 1.0 indicates higher throughput at the larger process count; a range including 1.0 means the measured rounds also include equal throughput.

| server | model | 16 procs | 32 procs | 32/16 (range) |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 40,909 | 48,752 | x1.19 (1.17–1.21) |
| RoadRunner | worker (warm, pipe) | 31,360 | 32,171 | x1.03 (1.01–1.04) |
| FrankenPHP | worker (warm, embedded) | 19,113 | 18,214 | x0.95 (0.93–1.55) — tie |
| volucer | spawn_cgi (cold, unix, children) | 10,718 | 12,607 | x1.18 (1.17–1.22) |
| nginx + php-fpm | cold, unix socket | 10,782 | 12,318 | x1.14 (1.12–1.15) |

The 64-process tier was measured but is not included in the throughput comparison. Its memory measurements are included below.

## What it costs in memory

Memory is measured after the run across the full process tree. The table reports **proportional set size (PSS)** in MiB. PSS apportions each shared memory page among the processes that use it, making it useful for comparing deployments with different process models.

RSS is shown in parentheses for reference. Summed across processes, RSS counts shared pages once for every process that maps them, while PSS divides their contribution among those processes.

![Proportional memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory-items-fast.svg)

| server | model | 16 procs | 32 procs | 64 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 82.0 (487) | 100.2 (878) | - |
| RoadRunner | worker (warm, pipe) | 420.5 (950) | 749.6 (1,837) | - |
| FrankenPHP | worker (warm, embedded) | 146.4 (146) | 170.0 (170) | - |
| volucer | spawn_cgi (cold, unix, children) | 73.3 (523) | 93.2 (973) | - |
| nginx + php-fpm | cold, unix socket | 63.6 (543) | 80.5 (1,003) | - |

Each value is PSS in MiB; RSS in MiB is shown in parentheses.

### The same PHP processes, arranged two ways

This comparison holds the number of serving PHP processes constant: one process per slot versus multiple forked children behind a slot.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes-items-fast.svg)

| shape | PHP processes | median PSS (MiB) | min | max | vs one per slot |
|---|---:|---:|---:|---:|---:|
| 8 separate slots | 8 | **193.8** | 193.6 | 194.6 | — |
| 1 slot × 8 children | 8 | **65.5** | 64.7 | 66.2 | 3.0× less |
| 16 separate slots | 16 | **340.3** | 339.9 | 340.9 | — |
| 1 slot × 16 children | 16 | **73.3** | 72.8 | 73.8 | 4.6× less |
| 32 separate slots | 32 | **633.7** | 633.0 | 634.3 | — |
| 1 slot × 32 children | 32 | **93.2** | 92.5 | 94.2 | 6.8× less |

The **vs one per slot** value is the separate-slot median divided by the forked-pool median at the same process count. For example, `4.5× less` means the forked pool's median PSS is about one quarter of the separate-slot median.

Forked children can share memory pages inherited from their master. PSS accounts for this sharing, which is why the forked-pool and separate-slot arrangements can have different total PSS at the same number of serving processes.

## What was measured

These are the software versions recorded for this benchmark run. Results may differ with other versions or builds. The values come from the run metadata and raw measurements in [`volucer-competition`](https://github.com/sailantis/volucer-competition).

| what | version |
|---|---|
| PHP (CLI) | PHP 8.5.11 (cli) |
| PHP (CGI) | PHP 8.5.11 (cgi-fcgi) |
| PHP (FPM) | PHP 8.5.11 (fpm-fcgi) |
| Volucer | volucer 0.6.0 |
| Volucer commit | 132f560 |
| RoadRunner | 3.0.1 |
| FrankenPHP | v1.13.4 |
| nginx | nginx/1.24.0 (Ubuntu) (worker_processes 4) |
| Bench app commit | cb21f89 |

Each chart also identifies the host, measurement method, and software versions, so it can be understood independently of this page.
