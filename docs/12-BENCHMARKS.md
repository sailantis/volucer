# Benchmarks

A comparison of Volucer, RoadRunner, FrankenPHP, and nginx with PHP-FPM using a shared benchmark harness. Results include throughput, latency, and process-tree memory.

All values come from committed benchmark results in
[`volucer-competition`](https://github.com/sailantis/volucer-competition). Tables and
charts are generated from the same measurements. Missing successful measurements are
shown as `-`, not as zero. The repository includes the method, raw rows, and controls.

Measurements were interleaved on the same host using the same harness and PHP build.
Each chart identifies its host, method, and measured software versions.

## `GET /items` — the ORM page

Measurements were interleaved across server arms, process counts, and load levels on the same host. The order rotated between rounds to reduce ordering effects. **PHP procs** is the number of PHP processes serving requests and provides a resource-use context for each result.

With `spawn_cgi` and `children`, multiple PHP processes serve through one slot. The process-count column makes this configuration comparable with other arms that use one process per slot.

Each chart shows request rate in requests per second, with bars that start at zero for direct visual comparison.

The label on each bar is its throughput factor relative to the fastest server in that chart. The dashed line marks that reference. For example, `x2.5` means the server handled about 1/2.5 of the reference's requests per second. The reference can differ between charts.

The range shows the minimum and maximum of the round-paired throughput ratios, converted back to requests per second. Pairing measurements from the same round helps account for host variation during the run; the range describes the comparison rather than either server alone.

If a factor range includes 1.0, the measured range includes equal throughput. The table marks that result **tie** to distinguish it from a comparison whose observed range stays above or below 1.0.

### OPcache on

![OPcache on at c=8](images/benchmarks/benchmarks/benchmarks-items-on-c8.svg)
![OPcache on at c=16](images/benchmarks/benchmarks/benchmarks-items-on-c16.svg)
![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-items-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-items-on-c64.svg)

The **factor** compares each row with the fastest server in the table (`x1.0`). A factor of `x2.5` means about 40% of the reference throughput. The range shows the round-paired comparison; **tie** marks a range that includes 1.0.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 32 | **25,816** | 2.17 ms | 5.02 ms | x1.0 |
| volucer | attach (warm, unix relay) | 16 | **25,664** | 2.2 ms | 6.73 ms | x1.0 (0.99–1.02) — tie |
| volucer | worker (warm, pipe) | 16 | **24,199** | 2.35 ms | 7.36 ms | x1.1 (1.06–1.07) |
| volucer | worker (warm, unix socket) | 16 | **23,912** | 2.37 ms | 7.23 ms | x1.1 (1.07–1.10) |
| volucer | worker (warm, pipe) | 32 | **23,601** | 2.38 ms | 5.59 ms | x1.1 (1.07–1.11) |
| volucer | worker (warm, unix socket) | 32 | **22,563** | 2.51 ms | 5.83 ms | x1.1 (1.13–1.16) |
| RoadRunner | worker (warm, pipe) | 32 | **20,764** | 2.65 ms | 7.57 ms | x1.2 (1.23–1.24) |
| RoadRunner | worker (warm, pipe) | 16 | **19,802** | 2.86 ms | 5.52 ms | x1.3 (1.29–1.31) |
| FrankenPHP | worker (warm, embedded) | 16 | **14,733** | 3.81 ms | 8.35 ms | x1.8 (1.73–1.77) |
| FrankenPHP | worker (warm, embedded) | 32 | **13,310** | 4.02 ms | 14.82 ms | x1.9 (1.91–1.97) |
| nginx + php-fpm | cold, unix socket | 32 | **9,270** | 6.19 ms | 9.75 ms | x2.8 (2.74–2.84) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **9,113** | 6.36 ms | 12.89 ms | x2.8 (2.81–2.83) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **8,557** | 6.81 ms | 16.61 ms | x3.0 (3.02–3.05) |
| nginx + php-fpm | cold, unix socket | 16 | **8,282** | 7.03 ms | 9.52 ms | x3.1 (3.03–3.15) |

This table compares the two cold PHP process pools at equal process counts: `spawn_cgi` with `children` and nginx with php-fpm.

| PHP procs | server | model | rps | spawn_cgi ÷ php-fpm |
|---:|---|---|---:|---:|
| 16 | volucer | spawn_cgi (cold, unix, children) | **8,557** | x1.03 |
| 16 | nginx + php-fpm | cold, unix socket | **8,282** | — |
| 32 | volucer | spawn_cgi (cold, unix, children) | **9,113** | x0.98 |
| 32 | nginx + php-fpm | cold, unix socket | **9,270** | — |

The factor is `spawn_cgi` throughput divided by php-fpm throughput at the same process count. Values above 1.0 indicate higher measured throughput for `spawn_cgi`; values below 1.0 indicate higher throughput for php-fpm.

This table compares each arm at the smallest and largest published process counts. Ratios are paired by measurement round. A ratio above 1.0 indicates higher throughput at the larger process count; a range including 1.0 means the measured rounds also include equal throughput.

| server | model | 16 procs | 32 procs | 32/16 (range) |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 25,664 | 25,816 | x1.01 (0.99–1.02) |
| volucer | worker (warm, pipe) | 24,199 | 23,601 | x0.98 (0.96–0.99) |
| volucer | worker (warm, unix socket) | 23,912 | 22,563 | x0.94 (0.94–0.95) |
| RoadRunner | worker (warm, pipe) | 19,802 | 20,764 | x1.05 (1.04–1.05) |
| FrankenPHP | worker (warm, embedded) | 14,733 | 13,310 | x0.90 (0.88–0.91) |
| volucer | spawn_cgi (cold, unix, children) | 8,557 | 9,113 | x1.07 (1.07–1.08) |
| nginx + php-fpm | cold, unix socket | 8,282 | 9,270 | x1.12 (1.10–1.12) |

The 64-process tier was measured but is not included in the throughput comparison. Its memory measurements are included below.

## What it costs in memory

Memory is measured after the run across the full process tree. The table reports **proportional set size (PSS)** in MiB. PSS apportions each shared memory page among the processes that use it, making it useful for comparing deployments with different process models.

RSS is shown in parentheses for reference. Summed across processes, RSS counts shared pages once for every process that maps them, while PSS divides their contribution among those processes.

![Proportional memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory.svg)

| server | model | 16 procs | 32 procs | 64 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 67.0 (414) | 85.1 (760) | 117.6 (1,417) |
| volucer | worker (warm, pipe) | 311.6 (820) | 579.8 (1,620) | 1101.3 (3,201) |
| volucer | worker (warm, unix socket) | 310.7 (819) | 579.8 (1,618) | 1101.4 (3,201) |
| RoadRunner | worker (warm, pipe) | 374.1 (880) | 653.0 (1,690) | 1199.3 (3,306) |
| FrankenPHP | worker (warm, embedded) | 139.7 (140) | 164.7 (165) | 205.6 (206) |
| volucer | spawn_cgi (cold, unix, children) | 68.9 (490) | 89.0 (914) | 116.5 (1,701) |
| nginx + php-fpm | cold, unix socket | 60.8 (501) | 79.2 (926) | 115.6 (1,789) |

Each value is PSS in MiB; RSS in MiB is shown in parentheses.

### The same PHP processes, arranged two ways

This comparison holds the number of serving PHP processes constant: one process per slot versus multiple forked children behind a slot.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes.svg)

| shape | PHP processes | median PSS (MiB) | min | max | vs one per slot |
|---|---:|---:|---:|---:|---:|
| 16 separate slots | 16 | **309.8** | 309.8 | 311.6 | — |
| 1 slot × 16 children | 16 | **68.9** | 67.6 | 69.1 | 4.5× less |
| 32 separate slots | 32 | **574.4** | 573.8 | 576.7 | — |
| 1 slot × 32 children | 32 | **89.0** | 85.9 | 90.0 | 6.5× less |
| 64 separate slots | 64 | **1102.9** | 1101.0 | 1103.4 | — |
| 1 slot × 64 children | 64 | **116.5** | 115.3 | 116.7 | 9.5× less |

The **vs one per slot** value is the separate-slot median divided by the forked-pool median at the same process count. For example, `4.5× less` means the forked pool's median PSS is about one quarter of the separate-slot median.

Forked children can share memory pages inherited from their master. PSS accounts for this sharing, which is why the forked-pool and separate-slot arrangements can have different total PSS at the same number of serving processes.

## What was measured

These are the software versions recorded for this benchmark run. Results may differ with other versions or builds. The values come from the run metadata and raw measurements in [`volucer-competition`](https://github.com/sailantis/volucer-competition).

| what | version |
|---|---|
| PHP (CLI) | PHP 8.5.11 (cli) |
| PHP (CGI) | PHP 8.5.11 (cgi-fcgi) |
| PHP (FPM) | PHP 8.5.11 (fpm-fcgi) |
| Volucer | volucer 0.1.0 |
| Volucer commit | c3da7ff |
| RoadRunner | 2025.1.15 |
| FrankenPHP | v1.13.0 |
| nginx | nginx/1.24.0 (Ubuntu) |
| Bench app commit | a4d44df |

Each chart also identifies the host, measurement method, and software versions, so it can be understood independently of this page.
