# Benchmarks

A comparison of Volucer, RoadRunner, FrankenPHP, and nginx with PHP-FPM using a shared benchmark harness. Results include throughput, latency, and process-tree memory.

All values come from committed benchmark results in
[`volucer-competition`](https://github.com/sailantis/volucer-competition). Tables and
charts are generated from the same measurements. Missing successful measurements are
shown as `-`, not as zero. The repository includes the method, raw rows, and controls.

Measurements were interleaved on the same host using the same harness and PHP build.
Each chart identifies its host, method, and measured software versions.

## C1 — Echo — `GET /echo`

Measurements were interleaved across server arms, sizing numbers, and load levels on the same host. The order rotated between rounds to reduce ordering effects. **PHP procs** is how many PHP processes serve requests, and it provides a resource-use context for each result.

With `spawn_cgi` and `children`, multiple PHP processes serve through one slot. The sizing column makes this configuration comparable with other arms that use one process per slot.

One row's number is not a process count: FrankenPHP runs a single OS process, so its value is how many worker *threads* that process starts. The row names its own unit. The two are compared because both are what the host pays for.

Each chart shows request rate in requests per second, with bars that start at zero for direct visual comparison.

The label on each bar is its throughput factor relative to the fastest server in that chart. The dashed line marks that reference. For example, `x2.5` means the server handled about 1/2.5 of the reference's requests per second. The reference can differ between charts.

The range is the spread of the comparison: the minimum and maximum of the round-paired throughput ratios, converted back to requests per second. Pairing measurements from the same round helps account for host variation during the run; the range describes the comparison rather than either server alone.

If a factor range includes 1.0, the measured range includes equal throughput. The table marks that result **tie** to distinguish it from a comparison whose observed range stays above or below 1.0.

### OPcache on

![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-echo-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-echo-on-c64.svg)
![OPcache on at c=96](images/benchmarks/benchmarks/benchmarks-echo-on-c96.svg)

The **factor** compares each row with the fastest server in the table (`x1.0`). A factor of `x2.5` means about 40% of the reference throughput. The range shows the round-paired comparison; **tie** marks a range that includes 1.0.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 32 | **110,814** | 0.817 ms | 2.06 ms | x1.0 |
| volucer | worker (warm, pipe) | 32 | **103,594** | 0.837 ms | 3.07 ms | x1.1 (1.05–1.08) |
| volucer | worker (warm, pipe) | 16 | **97,369** | 0.92 ms | 3.29 ms | x1.1 (1.12–1.21) |
| volucer | worker (warm, unix socket) | 32 | **88,744** | 1.03 ms | 2.62 ms | x1.2 (1.24–1.27) |
| volucer | attach (warm, unix relay) | 16 | **86,237** | 1.08 ms | 3.28 ms | x1.3 (1.27–1.31) |
| volucer | worker (warm, pipe) | 8 | **74,377** | 1.26 ms | 5.18 ms | x1.5 (1.42–1.55) |
| RoadRunner | worker (warm, pipe) | 32 | **73,761** | 1.16 ms | 3.67 ms | x1.5 (1.48–1.53) |
| FrankenPHP | worker (warm, embedded) | 8 threads | **73,458** | 1.16 ms | 4.32 ms | x1.5 (1.48–1.53) |
| volucer | worker (warm, unix socket) | 16 | **73,125** | 1.28 ms | 3.83 ms | x1.5 (1.51–1.53) |
| RoadRunner | worker (warm, pipe) | 16 | **69,226** | 1.3 ms | 2.73 ms | x1.6 (1.58–1.62) |
| volucer | attach (warm, unix relay) | 8 | **58,798** | 1.63 ms | 6.17 ms | x1.9 (1.86–1.89) |
| volucer | worker (warm, unix socket) | 8 | **53,641** | 1.79 ms | 6.69 ms | x2.1 (1.96–2.15) |
| RoadRunner | worker (warm, pipe) | 8 | **52,031** | 1.76 ms | 3.02 ms | x2.1 (2.10–2.15) |
| FrankenPHP | worker (warm, embedded) | 16 threads | **50,454** | 1.77 ms | 5.32 ms | x2.2 (2.17–2.43) |
| FrankenPHP | worker (warm, embedded) | 32 threads | **44,952** | 1.92 ms | 5.9 ms | x2.5 (2.43–2.48) |
| nginx + php-fpm | cold, unix socket | 32 | **40,791** | 2.25 ms | 4.2 ms | x2.7 (2.68–2.75) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **40,061** | 2.32 ms | 4.87 ms | x2.8 (2.75–2.78) |
| nginx + php-fpm | cold, unix socket | 16 | **37,694** | 2.51 ms | 3.32 ms | x2.9 (2.86–2.95) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **33,899** | 2.81 ms | 7.23 ms | x3.3 (3.23–3.38) |
| nginx + php-fpm | cold, unix socket | 8 | **33,016** | 2.87 ms | 3.66 ms | x3.4 (3.25–3.37) |
| volucer | spawn_cgi (cold, unix, children) | 8 | **26,345** | 3.6 ms | 11.2 ms | x4.2 (4.08–4.34) |

This table compares the two cold PHP process pools head to head at equal process counts: `spawn_cgi` with `children` and nginx with php-fpm.

| PHP procs | server | model | rps | spawn_cgi ÷ php-fpm |
|---:|---|---|---:|---:|
| 8 | volucer | spawn_cgi (cold, unix, children) | **26,345** | x0.80 |
| 8 | nginx + php-fpm | cold, unix socket | **33,016** | — |
| 16 | volucer | spawn_cgi (cold, unix, children) | **33,899** | x0.90 |
| 16 | nginx + php-fpm | cold, unix socket | **37,694** | — |
| 32 | volucer | spawn_cgi (cold, unix, children) | **40,061** | x0.98 |
| 32 | nginx + php-fpm | cold, unix socket | **40,791** | — |

The factor is `spawn_cgi` throughput divided by php-fpm throughput at the same process count. Values above 1.0 indicate higher measured throughput for `spawn_cgi`; values below 1.0 indicate higher throughput for php-fpm.

This table compares each arm across each step between consecutive published sizing numbers, so an arm that peaks in the middle is visible as a step of its own. Ratios are paired by measurement round. A ratio above 1.0 indicates higher throughput at the larger number, and below 1.0 higher throughput at the smaller one; a range including 1.0 means the measured rounds also include equal throughput.

| server | model | step (PHP procs) | rps at smaller | rps at larger | larger/smaller (range) |
|---|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 8 → 16 | 58,798 | 86,237 | x1.47 (1.44–1.48) |
| volucer | attach (warm, unix relay) | 16 → 32 | 86,237 | 110,814 | x1.29 (1.27–1.31) |
| volucer | worker (warm, pipe) | 8 → 16 | 74,377 | 97,369 | x1.31 (1.24–1.38) |
| volucer | worker (warm, pipe) | 16 → 32 | 97,369 | 103,594 | x1.06 (1.06–1.12) |
| volucer | worker (warm, unix socket) | 8 → 16 | 53,641 | 73,125 | x1.36 (1.30–1.40) |
| volucer | worker (warm, unix socket) | 16 → 32 | 73,125 | 88,744 | x1.21 (1.20–1.22) |
| RoadRunner | worker (warm, pipe) | 8 → 16 | 52,031 | 69,226 | x1.33 (1.32–1.34) |
| RoadRunner | worker (warm, pipe) | 16 → 32 | 69,226 | 73,761 | x1.07 (1.05–1.08) |
| FrankenPHP | worker (warm, embedded) | 8 → 16 threads | 73,458 | 50,454 | x0.69 (0.61–0.70) |
| FrankenPHP | worker (warm, embedded) | 16 → 32 threads | 50,454 | 44,952 | x0.89 (0.88–1.00) — tie |
| volucer | spawn_cgi (cold, unix, children) | 8 → 16 | 26,345 | 33,899 | x1.29 (1.25–1.32) |
| volucer | spawn_cgi (cold, unix, children) | 16 → 32 | 33,899 | 40,061 | x1.18 (1.16–1.23) |
| nginx + php-fpm | cold, unix socket | 8 → 16 | 33,016 | 37,694 | x1.14 (1.10–1.17) |
| nginx + php-fpm | cold, unix socket | 16 → 32 | 37,694 | 40,791 | x1.08 (1.07–1.09) |

## What it costs in memory

Memory is measured after the run across the full process tree. The table reports **Proportional set size (PSS)** in MiB. PSS apportions each shared memory page among the processes that use it, making it useful for comparing deployments with different process models.

RSS is shown in parentheses for reference. Summed across processes, RSS counts shared pages once for every process that maps them, while PSS divides their contribution among those processes.

![Proportional memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory-echo.svg)

| server | model | 8 procs | 16 procs | 32 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 66.2 (273) | 79.8 (484) | 99.1 (880) |
| volucer | worker (warm, pipe) | 216.5 (473) | 387.3 (921) | 715.2 (1,805) |
| volucer | worker (warm, unix socket) | 209.5 (466) | 376.4 (909) | 699.0 (1,789) |
| RoadRunner | worker (warm, pipe) | 251.3 (505) | 418.2 (950) | 745.7 (1,835) |
| FrankenPHP | worker (warm, embedded), threads | 130.8 (131) | 150.0 (150) | 173.5 (174) |
| volucer | spawn_cgi (cold, unix, children) | 67.6 (304) | 74.1 (527) | 94.1 (977) |
| nginx + php-fpm | cold, unix socket | 52.2 (313) | 61.6 (542) | 79.4 (1,005) |

Each value is PSS in MiB; RSS in MiB is shown in parentheses.

### The same PHP processes, arranged two ways

This comparison holds the number of serving PHP processes constant and changes only how each process came to exist: one boot per process, or one boot forked into every process.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes-echo.svg)

| pair | PHP processes | booted per process | forked | forked vs booted |
|---|---:|---:|---:|---:|
| warm pool: worker → attach | 8 | 216.5 | **66.2** | **3.3× less** |
|  | 16 | 387.3 | **79.8** | **4.9× less** |
|  | 32 | 715.2 | **99.1** | **7.2× less** |
| warm pool: unix → attach | 8 | 209.5 | **66.2** | **3.2× less** |
|  | 16 | 376.4 | **79.8** | **4.7× less** |
|  | 32 | 699.0 | **99.1** | **7.1× less** |

Both columns are PSS in MiB for the same number of serving PHP processes. The **forked vs booted** value is the booted median divided by the forked one, so `more` means forking *cost* memory at that count rather than saving it.

Forked children share memory pages inherited from their master, and PSS accounts for that sharing — which is why two arrangements with the same number of serving processes can have very different totals. How much it saves is a property of the *pair*: it grows with the process count above, because the boot each booted process pays is a fixed cost and the forked pool pays it once no matter how many children it has.

`worker` and `attach` differ in transport as well as in boot model — a pipe against a socket — so the warm pair is not single-variable by construction. It does not need to be, because the transport's own contribution is measured beside it: `unix` is the same server as `worker` over a socket, so the gap between those two rows is the transport's share of the footprint and the gap from `unix` to `attach` is the boot model's. Read together, they are why the wire is not the explanation — the second gap is larger than the first at every process count.

## C2 — Home — `GET /`

Measurements were interleaved across server arms, sizing numbers, and load levels on the same host. The order rotated between rounds to reduce ordering effects. **PHP procs** is how many PHP processes serve requests, and it provides a resource-use context for each result.

With `spawn_cgi` and `children`, multiple PHP processes serve through one slot. The sizing column makes this configuration comparable with other arms that use one process per slot.

One row's number is not a process count: FrankenPHP runs a single OS process, so its value is how many worker *threads* that process starts. The row names its own unit. The two are compared because both are what the host pays for.

Each chart shows request rate in requests per second, with bars that start at zero for direct visual comparison.

The label on each bar is its throughput factor relative to the fastest server in that chart. The dashed line marks that reference. For example, `x2.5` means the server handled about 1/2.5 of the reference's requests per second. The reference can differ between charts.

The range is the spread of the comparison: the minimum and maximum of the round-paired throughput ratios, converted back to requests per second. Pairing measurements from the same round helps account for host variation during the run; the range describes the comparison rather than either server alone.

If a factor range includes 1.0, the measured range includes equal throughput. The table marks that result **tie** to distinguish it from a comparison whose observed range stays above or below 1.0.

### OPcache on

![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-root-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-root-on-c64.svg)
![OPcache on at c=96](images/benchmarks/benchmarks/benchmarks-root-on-c96.svg)

The **factor** compares each row with the fastest server in the table (`x1.0`). A factor of `x2.5` means about 40% of the reference throughput. The range shows the round-paired comparison; **tie** marks a range that includes 1.0.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 32 | **96,858** | 0.94 ms | 2.31 ms | x1.0 |
| volucer | worker (warm, pipe) | 32 | **86,094** | 1.01 ms | 3.52 ms | x1.1 (1.09–1.14) |
| volucer | worker (warm, pipe) | 16 | **84,900** | 1.06 ms | 3.68 ms | x1.1 (1.10–1.17) |
| volucer | worker (warm, unix socket) | 32 | **75,526** | 1.21 ms | 3.03 ms | x1.3 (1.27–1.30) |
| volucer | attach (warm, unix relay) | 16 | **74,002** | 1.25 ms | 3.75 ms | x1.3 (1.28–1.33) |
| volucer | worker (warm, pipe) | 8 | **65,077** | 1.46 ms | 6.02 ms | x1.5 (1.45–1.59) |
| volucer | worker (warm, unix socket) | 16 | **62,277** | 1.5 ms | 4.39 ms | x1.6 (1.53–1.59) |
| RoadRunner | worker (warm, pipe) | 32 | **56,905** | 1.52 ms | 4.43 ms | x1.7 (1.67–1.73) |
| RoadRunner | worker (warm, pipe) | 16 | **54,365** | 1.64 ms | 3.4 ms | x1.8 (1.78–1.81) |
| volucer | attach (warm, unix relay) | 8 | **51,834** | 1.86 ms | 7.06 ms | x1.9 (1.80–1.89) |
| volucer | worker (warm, unix socket) | 8 | **47,332** | 2.04 ms | 7.61 ms | x2.0 (1.98–2.09) |
| RoadRunner | worker (warm, pipe) | 8 | **43,601** | 2.08 ms | 3.47 ms | x2.2 (2.20–2.22) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **30,807** | 3.05 ms | 6.21 ms | x3.1 (3.08–3.19) |
| FrankenPHP | worker (warm, embedded) | 8 threads | **30,671** | 2.93 ms | 5.28 ms | x3.2 (3.10–3.24) |
| nginx + php-fpm | cold, unix socket | 32 | **30,180** | 3.07 ms | 5.11 ms | x3.2 (3.16–3.24) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **26,989** | 3.52 ms | 9.18 ms | x3.6 (3.50–3.65) |
| nginx + php-fpm | cold, unix socket | 16 | **26,765** | 3.54 ms | 4.62 ms | x3.6 (3.59–3.68) |
| FrankenPHP | worker (warm, embedded) | 16 threads | **25,694** | 3.47 ms | 6.93 ms | x3.8 (3.72–3.82) |
| FrankenPHP | worker (warm, embedded) | 32 threads | **24,347** | 3.56 ms | 10.16 ms | x4.0 (3.92–10.82) |
| nginx + php-fpm | cold, unix socket | 8 | **22,972** | 4.12 ms | 5.32 ms | x4.2 (4.12–4.38) |
| volucer | spawn_cgi (cold, unix, children) | 8 | **20,837** | 4.56 ms | 13.97 ms | x4.6 (4.58–4.75) |

This table compares the two cold PHP process pools head to head at equal process counts: `spawn_cgi` with `children` and nginx with php-fpm.

| PHP procs | server | model | rps | spawn_cgi ÷ php-fpm |
|---:|---|---|---:|---:|
| 8 | volucer | spawn_cgi (cold, unix, children) | **20,837** | x0.91 |
| 8 | nginx + php-fpm | cold, unix socket | **22,972** | — |
| 16 | volucer | spawn_cgi (cold, unix, children) | **26,989** | x1.01 |
| 16 | nginx + php-fpm | cold, unix socket | **26,765** | — |
| 32 | volucer | spawn_cgi (cold, unix, children) | **30,807** | x1.02 |
| 32 | nginx + php-fpm | cold, unix socket | **30,180** | — |

The factor is `spawn_cgi` throughput divided by php-fpm throughput at the same process count. Values above 1.0 indicate higher measured throughput for `spawn_cgi`; values below 1.0 indicate higher throughput for php-fpm.

This table compares each arm across each step between consecutive published sizing numbers, so an arm that peaks in the middle is visible as a step of its own. Ratios are paired by measurement round. A ratio above 1.0 indicates higher throughput at the larger number, and below 1.0 higher throughput at the smaller one; a range including 1.0 means the measured rounds also include equal throughput.

| server | model | step (PHP procs) | rps at smaller | rps at larger | larger/smaller (range) |
|---|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 8 → 16 | 51,834 | 74,002 | x1.43 (1.38–1.47) |
| volucer | attach (warm, unix relay) | 16 → 32 | 74,002 | 96,858 | x1.31 (1.28–1.33) |
| volucer | worker (warm, pipe) | 8 → 16 | 65,077 | 84,900 | x1.30 (1.29–1.35) |
| volucer | worker (warm, pipe) | 16 → 32 | 84,900 | 86,094 | x1.01 (0.98–1.05) — tie |
| volucer | worker (warm, unix socket) | 8 → 16 | 47,332 | 62,277 | x1.32 (1.28–1.35) |
| volucer | worker (warm, unix socket) | 16 → 32 | 62,277 | 75,526 | x1.21 (1.20–1.23) |
| RoadRunner | worker (warm, pipe) | 8 → 16 | 43,601 | 54,365 | x1.25 (1.22–1.25) |
| RoadRunner | worker (warm, pipe) | 16 → 32 | 54,365 | 56,905 | x1.05 (1.03–1.07) |
| FrankenPHP | worker (warm, embedded) | 8 → 16 threads | 30,671 | 25,694 | x0.84 (0.81–0.85) |
| FrankenPHP | worker (warm, embedded) | 16 → 32 threads | 25,694 | 24,347 | x0.95 (0.35–0.96) |
| volucer | spawn_cgi (cold, unix, children) | 8 → 16 | 20,837 | 26,989 | x1.30 (1.26–1.31) |
| volucer | spawn_cgi (cold, unix, children) | 16 → 32 | 26,989 | 30,807 | x1.14 (1.13–1.16) |
| nginx + php-fpm | cold, unix socket | 8 → 16 | 22,972 | 26,765 | x1.17 (1.13–1.22) |
| nginx + php-fpm | cold, unix socket | 16 → 32 | 26,765 | 30,180 | x1.13 (1.12–1.15) |

## What it costs in memory

Memory is measured after the run across the full process tree. The table reports **Proportional set size (PSS)** in MiB. PSS apportions each shared memory page among the processes that use it, making it useful for comparing deployments with different process models.

RSS is shown in parentheses for reference. Summed across processes, RSS counts shared pages once for every process that maps them, while PSS divides their contribution among those processes.

![Proportional memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory-root.svg)

| server | model | 8 procs | 16 procs | 32 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 66.2 (273) | 79.8 (484) | 99.1 (880) |
| volucer | worker (warm, pipe) | 216.5 (473) | 387.3 (921) | 715.2 (1,805) |
| volucer | worker (warm, unix socket) | 209.5 (466) | 376.4 (909) | 699.0 (1,789) |
| RoadRunner | worker (warm, pipe) | 251.3 (505) | 418.2 (950) | 745.7 (1,835) |
| FrankenPHP | worker (warm, embedded), threads | 130.8 (131) | 150.0 (150) | 173.5 (174) |
| volucer | spawn_cgi (cold, unix, children) | 67.6 (304) | 74.1 (527) | 94.1 (977) |
| nginx + php-fpm | cold, unix socket | 52.2 (313) | 61.6 (542) | 79.4 (1,005) |

Each value is PSS in MiB; RSS in MiB is shown in parentheses.

### The same PHP processes, arranged two ways

This comparison holds the number of serving PHP processes constant and changes only how each process came to exist: one boot per process, or one boot forked into every process.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes-root.svg)

| pair | PHP processes | booted per process | forked | forked vs booted |
|---|---:|---:|---:|---:|
| warm pool: worker → attach | 8 | 216.5 | **66.2** | **3.3× less** |
|  | 16 | 387.3 | **79.8** | **4.9× less** |
|  | 32 | 715.2 | **99.1** | **7.2× less** |
| warm pool: unix → attach | 8 | 209.5 | **66.2** | **3.2× less** |
|  | 16 | 376.4 | **79.8** | **4.7× less** |
|  | 32 | 699.0 | **99.1** | **7.1× less** |

Both columns are PSS in MiB for the same number of serving PHP processes. The **forked vs booted** value is the booted median divided by the forked one, so `more` means forking *cost* memory at that count rather than saving it.

Forked children share memory pages inherited from their master, and PSS accounts for that sharing — which is why two arrangements with the same number of serving processes can have very different totals. How much it saves is a property of the *pair*: it grows with the process count above, because the boot each booted process pays is a fixed cost and the forked pool pays it once no matter how many children it has.

`worker` and `attach` differ in transport as well as in boot model — a pipe against a socket — so the warm pair is not single-variable by construction. It does not need to be, because the transport's own contribution is measured beside it: `unix` is the same server as `worker` over a socket, so the gap between those two rows is the transport's share of the footprint and the gap from `unix` to `attach` is the boot model's. Read together, they are why the wire is not the explanation — the second gap is larger than the first at every process count.

## C6 — API Items — `GET /api/items-fast`

Measurements were interleaved across server arms, sizing numbers, and load levels on the same host. The order rotated between rounds to reduce ordering effects. **PHP procs** is how many PHP processes serve requests, and it provides a resource-use context for each result.

With `spawn_cgi` and `children`, multiple PHP processes serve through one slot. The sizing column makes this configuration comparable with other arms that use one process per slot.

One row's number is not a process count: FrankenPHP runs a single OS process, so its value is how many worker *threads* that process starts. The row names its own unit. The two are compared because both are what the host pays for.

Each chart shows request rate in requests per second, with bars that start at zero for direct visual comparison.

The label on each bar is its throughput factor relative to the fastest server in that chart. The dashed line marks that reference. For example, `x2.5` means the server handled about 1/2.5 of the reference's requests per second. The reference can differ between charts.

The range is the spread of the comparison: the minimum and maximum of the round-paired throughput ratios, converted back to requests per second. Pairing measurements from the same round helps account for host variation during the run; the range describes the comparison rather than either server alone.

If a factor range includes 1.0, the measured range includes equal throughput. The table marks that result **tie** to distinguish it from a comparison whose observed range stays above or below 1.0.

### OPcache on

![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-api-items-fast-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-api-items-fast-on-c64.svg)
![OPcache on at c=96](images/benchmarks/benchmarks/benchmarks-api-items-fast-on-c96.svg)

The **factor** compares each row with the fastest server in the table (`x1.0`). A factor of `x2.5` means about 40% of the reference throughput. The range shows the round-paired comparison; **tie** marks a range that includes 1.0.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 32 | **72,280** | 1.25 ms | 3.11 ms | x1.0 |
| volucer | worker (warm, pipe) | 32 | **63,378** | 1.41 ms | 4.07 ms | x1.1 (1.11–1.17) |
| volucer | worker (warm, pipe) | 16 | **62,434** | 1.46 ms | 5.07 ms | x1.2 (1.11–1.18) |
| FrankenPHP | worker (warm, embedded) | 8 threads | **61,900** | 1.37 ms | 4.82 ms | x1.2 (1.15–1.19) |
| volucer | attach (warm, unix relay) | 16 | **55,078** | 1.7 ms | 4.82 ms | x1.3 (1.27–1.32) |
| volucer | worker (warm, unix socket) | 32 | **54,729** | 1.68 ms | 3.93 ms | x1.3 (1.30–1.37) |
| volucer | worker (warm, pipe) | 8 | **47,395** | 2.04 ms | 8.62 ms | x1.5 (1.50–1.55) |
| volucer | worker (warm, unix socket) | 16 | **47,224** | 2.01 ms | 5.6 ms | x1.5 (1.49–1.56) |
| RoadRunner | worker (warm, pipe) | 16 | **45,852** | 1.98 ms | 3.75 ms | x1.6 (1.57–1.59) |
| RoadRunner | worker (warm, pipe) | 32 | **45,267** | 1.98 ms | 4.67 ms | x1.6 (1.57–1.60) |
| volucer | attach (warm, unix relay) | 8 | **42,712** | 2.26 ms | 8.17 ms | x1.7 (1.65–1.77) |
| FrankenPHP | worker (warm, embedded) | 16 threads | **41,100** | 2.22 ms | 6.51 ms | x1.8 (1.74–1.78) |
| volucer | worker (warm, unix socket) | 8 | **39,974** | 2.41 ms | 8.78 ms | x1.8 (1.70–1.88) |
| RoadRunner | worker (warm, pipe) | 8 | **37,898** | 2.44 ms | 3.89 ms | x1.9 (1.87–1.93) |
| FrankenPHP | worker (warm, embedded) | 32 threads | **22,606** | 2.5 ms | 7.33 ms | x3.2 (2.09–36.48) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **15,360** | 6.15 ms | 12.81 ms | x4.7 (4.62–4.75) |
| nginx + php-fpm | cold, unix socket | 32 | **14,697** | 6.38 ms | 9.15 ms | x4.9 (4.82–4.94) |
| nginx + php-fpm | cold, unix socket | 16 | **12,906** | 7.36 ms | 9.33 ms | x5.6 (5.55–5.70) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **12,691** | 7.49 ms | 19.52 ms | x5.7 (5.58–5.85) |
| nginx + php-fpm | cold, unix socket | 8 | **9,564** | 9.96 ms | 11.88 ms | x7.6 (7.45–7.68) |
| volucer | spawn_cgi (cold, unix, children) | 8 | **9,346** | 10.22 ms | 29.18 ms | x7.7 (7.52–7.87) |

This table compares the two cold PHP process pools head to head at equal process counts: `spawn_cgi` with `children` and nginx with php-fpm.

| PHP procs | server | model | rps | spawn_cgi ÷ php-fpm |
|---:|---|---|---:|---:|
| 8 | volucer | spawn_cgi (cold, unix, children) | **9,346** | x0.98 |
| 8 | nginx + php-fpm | cold, unix socket | **9,564** | — |
| 16 | volucer | spawn_cgi (cold, unix, children) | **12,691** | x0.98 |
| 16 | nginx + php-fpm | cold, unix socket | **12,906** | — |
| 32 | volucer | spawn_cgi (cold, unix, children) | **15,360** | x1.05 |
| 32 | nginx + php-fpm | cold, unix socket | **14,697** | — |

The factor is `spawn_cgi` throughput divided by php-fpm throughput at the same process count. Values above 1.0 indicate higher measured throughput for `spawn_cgi`; values below 1.0 indicate higher throughput for php-fpm.

This table compares each arm across each step between consecutive published sizing numbers, so an arm that peaks in the middle is visible as a step of its own. Ratios are paired by measurement round. A ratio above 1.0 indicates higher throughput at the larger number, and below 1.0 higher throughput at the smaller one; a range including 1.0 means the measured rounds also include equal throughput.

| server | model | step (PHP procs) | rps at smaller | rps at larger | larger/smaller (range) |
|---|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 8 → 16 | 42,712 | 55,078 | x1.29 (1.25–1.37) |
| volucer | attach (warm, unix relay) | 16 → 32 | 55,078 | 72,280 | x1.31 (1.27–1.32) |
| volucer | worker (warm, pipe) | 8 → 16 | 47,395 | 62,434 | x1.32 (1.29–1.36) |
| volucer | worker (warm, pipe) | 16 → 32 | 62,434 | 63,378 | x1.02 (0.99–1.03) — tie |
| volucer | worker (warm, unix socket) | 8 → 16 | 39,974 | 47,224 | x1.18 (1.14–1.24) |
| volucer | worker (warm, unix socket) | 16 → 32 | 47,224 | 54,729 | x1.16 (1.14–1.16) |
| RoadRunner | worker (warm, pipe) | 8 → 16 | 37,898 | 45,852 | x1.21 (1.19–1.22) |
| RoadRunner | worker (warm, pipe) | 16 → 32 | 45,852 | 45,267 | x0.99 (0.98–1.01) — tie |
| FrankenPHP | worker (warm, embedded) | 8 → 16 threads | 61,900 | 41,100 | x0.66 (0.66–0.68) |
| FrankenPHP | worker (warm, embedded) | 16 → 32 threads | 41,100 | 22,606 | x0.55 (0.05–0.83) |
| volucer | spawn_cgi (cold, unix, children) | 8 → 16 | 9,346 | 12,691 | x1.36 (1.34–1.36) |
| volucer | spawn_cgi (cold, unix, children) | 16 → 32 | 12,691 | 15,360 | x1.21 (1.20–1.23) |
| nginx + php-fpm | cold, unix socket | 8 → 16 | 9,564 | 12,906 | x1.35 (1.33–1.37) |
| nginx + php-fpm | cold, unix socket | 16 → 32 | 12,906 | 14,697 | x1.14 (1.13–1.16) |

## What it costs in memory

Memory is measured after the run across the full process tree. The table reports **Proportional set size (PSS)** in MiB. PSS apportions each shared memory page among the processes that use it, making it useful for comparing deployments with different process models.

RSS is shown in parentheses for reference. Summed across processes, RSS counts shared pages once for every process that maps them, while PSS divides their contribution among those processes.

![Proportional memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory-api-items-fast.svg)

| server | model | 8 procs | 16 procs | 32 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 66.2 (273) | 79.8 (484) | 99.1 (880) |
| volucer | worker (warm, pipe) | 216.5 (473) | 387.3 (921) | 715.2 (1,805) |
| volucer | worker (warm, unix socket) | 209.5 (466) | 376.4 (909) | 699.0 (1,789) |
| RoadRunner | worker (warm, pipe) | 251.3 (505) | 418.2 (950) | 745.7 (1,835) |
| FrankenPHP | worker (warm, embedded), threads | 130.8 (131) | 150.0 (150) | 173.5 (174) |
| volucer | spawn_cgi (cold, unix, children) | 67.6 (304) | 74.1 (527) | 94.1 (977) |
| nginx + php-fpm | cold, unix socket | 52.2 (313) | 61.6 (542) | 79.4 (1,005) |

Each value is PSS in MiB; RSS in MiB is shown in parentheses.

### The same PHP processes, arranged two ways

This comparison holds the number of serving PHP processes constant and changes only how each process came to exist: one boot per process, or one boot forked into every process.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes-api-items-fast.svg)

| pair | PHP processes | booted per process | forked | forked vs booted |
|---|---:|---:|---:|---:|
| warm pool: worker → attach | 8 | 216.5 | **66.2** | **3.3× less** |
|  | 16 | 387.3 | **79.8** | **4.9× less** |
|  | 32 | 715.2 | **99.1** | **7.2× less** |
| warm pool: unix → attach | 8 | 209.5 | **66.2** | **3.2× less** |
|  | 16 | 376.4 | **79.8** | **4.7× less** |
|  | 32 | 699.0 | **99.1** | **7.1× less** |

Both columns are PSS in MiB for the same number of serving PHP processes. The **forked vs booted** value is the booted median divided by the forked one, so `more` means forking *cost* memory at that count rather than saving it.

Forked children share memory pages inherited from their master, and PSS accounts for that sharing — which is why two arrangements with the same number of serving processes can have very different totals. How much it saves is a property of the *pair*: it grows with the process count above, because the boot each booted process pays is a fixed cost and the forked pool pays it once no matter how many children it has.

`worker` and `attach` differ in transport as well as in boot model — a pipe against a socket — so the warm pair is not single-variable by construction. It does not need to be, because the transport's own contribution is measured beside it: `unix` is the same server as `worker` over a socket, so the gap between those two rows is the transport's share of the footprint and the gap from `unix` to `attach` is the boot model's. Read together, they are why the wire is not the explanation — the second gap is larger than the first at every process count.

## C5 — HTML Items — `GET /items-fast`

Measurements were interleaved across server arms, sizing numbers, and load levels on the same host. The order rotated between rounds to reduce ordering effects. **PHP procs** is how many PHP processes serve requests, and it provides a resource-use context for each result.

With `spawn_cgi` and `children`, multiple PHP processes serve through one slot. The sizing column makes this configuration comparable with other arms that use one process per slot.

One row's number is not a process count: FrankenPHP runs a single OS process, so its value is how many worker *threads* that process starts. The row names its own unit. The two are compared because both are what the host pays for.

Each chart shows request rate in requests per second, with bars that start at zero for direct visual comparison.

The label on each bar is its throughput factor relative to the fastest server in that chart. The dashed line marks that reference. For example, `x2.5` means the server handled about 1/2.5 of the reference's requests per second. The reference can differ between charts.

The range is the spread of the comparison: the minimum and maximum of the round-paired throughput ratios, converted back to requests per second. Pairing measurements from the same round helps account for host variation during the run; the range describes the comparison rather than either server alone.

If a factor range includes 1.0, the measured range includes equal throughput. The table marks that result **tie** to distinguish it from a comparison whose observed range stays above or below 1.0.

### OPcache on

![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-items-fast-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-items-fast-on-c64.svg)
![OPcache on at c=96](images/benchmarks/benchmarks/benchmarks-items-fast-on-c96.svg)

The **factor** compares each row with the fastest server in the table (`x1.0`). A factor of `x2.5` means about 40% of the reference throughput. The range shows the round-paired comparison; **tie** marks a range that includes 1.0.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 32 | **50,870** | 1.8 ms | 4.18 ms | x1.0 |
| volucer | worker (warm, pipe) | 16 | **44,945** | 2.07 ms | 7.08 ms | x1.1 (1.12–1.17) |
| volucer | worker (warm, pipe) | 32 | **43,995** | 2.06 ms | 5.23 ms | x1.2 (1.15–1.19) |
| volucer | attach (warm, unix relay) | 16 | **41,215** | 2.29 ms | 6.28 ms | x1.2 (1.22–1.28) |
| volucer | worker (warm, unix socket) | 32 | **39,976** | 2.31 ms | 5.1 ms | x1.3 (1.25–1.32) |
| volucer | worker (warm, unix socket) | 16 | **35,862** | 2.65 ms | 7.1 ms | x1.4 (1.38–1.44) |
| volucer | worker (warm, pipe) | 8 | **33,810** | 2.88 ms | 11.27 ms | x1.5 (1.48–1.55) |
| volucer | attach (warm, unix relay) | 8 | **32,741** | 2.94 ms | 10.42 ms | x1.6 (1.53–1.67) |
| RoadRunner | worker (warm, pipe) | 32 | **31,559** | 2.87 ms | 6.53 ms | x1.6 (1.60–1.65) |
| RoadRunner | worker (warm, pipe) | 16 | **30,713** | 2.98 ms | 5.15 ms | x1.7 (1.62–1.67) |
| volucer | worker (warm, unix socket) | 8 | **30,703** | 3.14 ms | 10.85 ms | x1.7 (1.60–1.69) |
| RoadRunner | worker (warm, pipe) | 8 | **26,927** | 3.47 ms | 5.12 ms | x1.9 (1.88–1.94) |
| FrankenPHP | worker (warm, embedded) | 8 threads | **23,374** | 3.94 ms | 6.31 ms | x2.2 (2.16–2.19) |
| FrankenPHP | worker (warm, embedded) | 16 threads | **19,011** | 4.81 ms | 8.63 ms | x2.7 (2.64–3.13) |
| FrankenPHP | worker (warm, embedded) | 32 threads | **18,039** | 4.89 ms | 13.5 ms | x2.8 (2.79–2.91) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **12,577** | 7.53 ms | 14.95 ms | x4.0 (3.96–4.15) |
| nginx + php-fpm | cold, unix socket | 32 | **12,105** | 7.77 ms | 10.81 ms | x4.2 (4.19–4.27) |
| nginx + php-fpm | cold, unix socket | 16 | **10,738** | 8.86 ms | 10.87 ms | x4.7 (4.70–4.84) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **10,615** | 8.96 ms | 23.52 ms | x4.8 (4.74–4.86) |
| volucer | spawn_cgi (cold, unix, children) | 8 | **8,345** | 11.49 ms | 32.38 ms | x6.1 (6.06–6.25) |
| nginx + php-fpm | cold, unix socket | 8 | **8,298** | 11.53 ms | 13.47 ms | x6.1 (6.09–6.21) |

This table compares the two cold PHP process pools head to head at equal process counts: `spawn_cgi` with `children` and nginx with php-fpm.

| PHP procs | server | model | rps | spawn_cgi ÷ php-fpm |
|---:|---|---|---:|---:|
| 8 | volucer | spawn_cgi (cold, unix, children) | **8,345** | x1.01 |
| 8 | nginx + php-fpm | cold, unix socket | **8,298** | — |
| 16 | volucer | spawn_cgi (cold, unix, children) | **10,615** | x0.99 |
| 16 | nginx + php-fpm | cold, unix socket | **10,738** | — |
| 32 | volucer | spawn_cgi (cold, unix, children) | **12,577** | x1.04 |
| 32 | nginx + php-fpm | cold, unix socket | **12,105** | — |

The factor is `spawn_cgi` throughput divided by php-fpm throughput at the same process count. Values above 1.0 indicate higher measured throughput for `spawn_cgi`; values below 1.0 indicate higher throughput for php-fpm.

This table compares each arm across each step between consecutive published sizing numbers, so an arm that peaks in the middle is visible as a step of its own. Ratios are paired by measurement round. A ratio above 1.0 indicates higher throughput at the larger number, and below 1.0 higher throughput at the smaller one; a range including 1.0 means the measured rounds also include equal throughput.

| server | model | step (PHP procs) | rps at smaller | rps at larger | larger/smaller (range) |
|---|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 8 → 16 | 32,741 | 41,215 | x1.26 (1.24–1.34) |
| volucer | attach (warm, unix relay) | 16 → 32 | 41,215 | 50,870 | x1.23 (1.22–1.28) |
| volucer | worker (warm, pipe) | 8 → 16 | 33,810 | 44,945 | x1.33 (1.30–1.33) |
| volucer | worker (warm, pipe) | 16 → 32 | 44,945 | 43,995 | x0.98 (0.95–1.00) — tie |
| volucer | worker (warm, unix socket) | 8 → 16 | 30,703 | 35,862 | x1.17 (1.14–1.22) |
| volucer | worker (warm, unix socket) | 16 → 32 | 35,862 | 39,976 | x1.11 (1.09–1.14) |
| RoadRunner | worker (warm, pipe) | 8 → 16 | 26,927 | 30,713 | x1.14 (1.14–1.17) |
| RoadRunner | worker (warm, pipe) | 16 → 32 | 30,713 | 31,559 | x1.03 (1.00–1.03) — tie |
| FrankenPHP | worker (warm, embedded) | 8 → 16 threads | 23,374 | 19,011 | x0.81 (0.69–0.83) |
| FrankenPHP | worker (warm, embedded) | 16 → 32 threads | 19,011 | 18,039 | x0.95 (0.92–1.11) — tie |
| volucer | spawn_cgi (cold, unix, children) | 8 → 16 | 8,345 | 10,615 | x1.27 (1.26–1.29) |
| volucer | spawn_cgi (cold, unix, children) | 16 → 32 | 10,615 | 12,577 | x1.18 (1.15–1.20) |
| nginx + php-fpm | cold, unix socket | 8 → 16 | 8,298 | 10,738 | x1.29 (1.27–1.30) |
| nginx + php-fpm | cold, unix socket | 16 → 32 | 10,738 | 12,105 | x1.13 (1.11–1.15) |

## What it costs in memory

Memory is measured after the run across the full process tree. The table reports **Proportional set size (PSS)** in MiB. PSS apportions each shared memory page among the processes that use it, making it useful for comparing deployments with different process models.

RSS is shown in parentheses for reference. Summed across processes, RSS counts shared pages once for every process that maps them, while PSS divides their contribution among those processes.

![Proportional memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory-items-fast.svg)

| server | model | 8 procs | 16 procs | 32 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 66.2 (273) | 79.8 (484) | 99.1 (880) |
| volucer | worker (warm, pipe) | 216.5 (473) | 387.3 (921) | 715.2 (1,805) |
| volucer | worker (warm, unix socket) | 209.5 (466) | 376.4 (909) | 699.0 (1,789) |
| RoadRunner | worker (warm, pipe) | 251.3 (505) | 418.2 (950) | 745.7 (1,835) |
| FrankenPHP | worker (warm, embedded), threads | 130.8 (131) | 150.0 (150) | 173.5 (174) |
| volucer | spawn_cgi (cold, unix, children) | 67.6 (304) | 74.1 (527) | 94.1 (977) |
| nginx + php-fpm | cold, unix socket | 52.2 (313) | 61.6 (542) | 79.4 (1,005) |

Each value is PSS in MiB; RSS in MiB is shown in parentheses.

### The same PHP processes, arranged two ways

This comparison holds the number of serving PHP processes constant and changes only how each process came to exist: one boot per process, or one boot forked into every process.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes-items-fast.svg)

| pair | PHP processes | booted per process | forked | forked vs booted |
|---|---:|---:|---:|---:|
| warm pool: worker → attach | 8 | 216.5 | **66.2** | **3.3× less** |
|  | 16 | 387.3 | **79.8** | **4.9× less** |
|  | 32 | 715.2 | **99.1** | **7.2× less** |
| warm pool: unix → attach | 8 | 209.5 | **66.2** | **3.2× less** |
|  | 16 | 376.4 | **79.8** | **4.7× less** |
|  | 32 | 699.0 | **99.1** | **7.1× less** |

Both columns are PSS in MiB for the same number of serving PHP processes. The **forked vs booted** value is the booted median divided by the forked one, so `more` means forking *cost* memory at that count rather than saving it.

Forked children share memory pages inherited from their master, and PSS accounts for that sharing — which is why two arrangements with the same number of serving processes can have very different totals. How much it saves is a property of the *pair*: it grows with the process count above, because the boot each booted process pays is a fixed cost and the forked pool pays it once no matter how many children it has.

`worker` and `attach` differ in transport as well as in boot model — a pipe against a socket — so the warm pair is not single-variable by construction. It does not need to be, because the transport's own contribution is measured beside it: `unix` is the same server as `worker` over a socket, so the gap between those two rows is the transport's share of the footprint and the gap from `unix` to `attach` is the boot model's. Read together, they are why the wire is not the explanation — the second gap is larger than the first at every process count.

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
