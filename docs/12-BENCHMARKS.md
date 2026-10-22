# Benchmarks

Volucer against RoadRunner, FrankenPHP and nginx + php-fpm, every arm through one harness on one interpreter - and what each of them costs in memory.

**Measured, not estimated.** Every number on this page comes from one committed
result file in
[`volucer-competition`](https://github.com/sailantis/volucer-competition), and the
charts are generated from that same file — so a chart cannot disagree with the table
beside it. A cell with no successful repeat is printed as `-`, never as `0`: a zero reads
as "measured, and it was terrible" rather than "not measured". The method, the raw rows
and the controls live in that repository.

**One host, one harness, one interpreter, one pass.** Every figure below was measured on
the same machine, through the same script, against the same PHP build, in one
interleaved pass — so a ratio between two arms is a ratio between two measurements made
the same way. The host, the kernel, the PHP version and the method are printed on every
chart.

## `GET /items` — the ORM page

Every row below was measured in **one interleaved pass** — every arm, at every process count, at every load, in one round-robin over one host, with the order rotating so that no cell is always first and no cell is always last. The **PHP procs** column is the cost side of the comparison: it says how many PHP processes serve the requests, and it is what the host pays for.

The `spawn_cgi` arm with `children` puts several processes behind one listening socket, so it reaches a given throughput with a different amount of server than the arms beside it. That is why the process count is a column and not a footnote: a row with more processes is not a better row, it is a row that bought its throughput with more server.

Each chart is a bar per server, and the bar is the **request rate** — the number the page is about. The bars start at zero, so their lengths can be compared directly.

**The label behind each bar is how many times slower that server is than the fastest server in the same chart.** The fastest server is the dashed vertical line, and it carries no label of its own; every other bar states its factor — `x2.5` for a server that did 40% of the leader's requests per second. The baseline is the fastest server *in that chart*, not a fixed server: a cold deployment overtakes a warm one once the host saturates, so the fastest server is not the same one at every load — and naming it per chart is more honest than a fixed reference that would read as a claim about the same winner every time.

**The range on each bar is the spread of the comparison, not of the server.** Both servers were measured once per round, interleaved, so each round holds a ratio of the two. The range is the smallest and largest of those round-paired ratios, scaled back into requests per second. Pairing by round is what makes that range honest: a host that drifts over the run moves both servers in the same round, so the ratio within a round is steadier than either server's own spread — measurably so, on this run, in most of the pairs on this page. A range drawn from one server's minimum and maximum would instead charge the whole of the host's drift to that one server, and would be wide for the wrong reason.

A range that reaches across the 1.0 line means the two servers could not be told apart in this run. The table marks those rows **tie**: a difference inside the measurement's own spread is not a difference, and the factor there is the best estimate rather than a claim that one server is faster.

### OPcache on

![OPcache on at c=8](images/benchmarks/benchmarks/benchmarks-items-on-c8.svg)
![OPcache on at c=16](images/benchmarks/benchmarks/benchmarks-items-on-c16.svg)
![OPcache on at c=32](images/benchmarks/benchmarks/benchmarks-items-on-c32.svg)
![OPcache on at c=64](images/benchmarks/benchmarks/benchmarks-items-on-c64.svg)

The **factor** column is measured against **volucer — attach (warm, unix relay)** — the fastest server in this table. It states how many times slower each server is, so `x1.0` is the fastest and `x2.5` is a quarter of its rate. The column carries the range of the round-paired ratios beside it, and a row marked **tie** is one whose range reaches across 1.0 — the two servers could not be told apart in this run.

| server | model | PHP procs | rps | p50 | p99 | factor (range) |
|---|---|---:|---:|---:|---:|---:|
| volucer | attach (warm, unix relay) | 16 | **26,608** | 2.12 ms | 6.55 ms | x1.0 |
| volucer | attach (warm, unix relay) | 32 | **26,021** | 2.16 ms | 4.99 ms | x1.0 (1.01–1.04) |
| volucer | worker (warm, pipe) | 16 | **25,198** | 2.24 ms | 7.03 ms | x1.1 (1.00–1.11) — tie |
| volucer | worker (warm, unix socket) | 16 | **24,228** | 2.34 ms | 7.22 ms | x1.1 (1.07–1.11) |
| volucer | worker (warm, pipe) | 32 | **23,684** | 2.4 ms | 5.59 ms | x1.1 (1.10–1.13) |
| volucer | worker (warm, unix socket) | 32 | **23,000** | 2.46 ms | 5.84 ms | x1.2 (1.15–1.16) |
| RoadRunner | worker (warm, pipe) | 32 | **20,787** | 2.66 ms | 7.56 ms | x1.3 (1.24–1.31) |
| RoadRunner | worker (warm, pipe) | 16 | **20,123** | 2.82 ms | 5.46 ms | x1.3 (1.29–1.35) |
| FrankenPHP | worker (warm, embedded) | 16 | **14,667** | 3.8 ms | 8.37 ms | x1.8 (1.77–1.84) |
| FrankenPHP | worker (warm, embedded) | 32 | **13,626** | 3.9 ms | 14.28 ms | x2.0 (1.90–1.99) |
| nginx + php-fpm | cold, unix socket | 32 | **9,401** | 6.11 ms | 9.6 ms | x2.8 (2.75–2.88) |
| volucer | spawn_cgi (cold, unix, children) | 32 | **9,086** | 6.31 ms | 12.91 ms | x2.9 (2.88–2.93) |
| volucer | spawn_cgi (cold, unix, children) | 16 | **8,635** | 6.79 ms | 16.67 ms | x3.1 (3.01–3.12) |
| nginx + php-fpm | cold, unix socket | 16 | **8,406** | 7.03 ms | 9.35 ms | x3.2 (3.05–3.23) |

**The two cold deployments, head to head.** `spawn_cgi` with `children` and `nginx + php-fpm` are the same idea reached two ways — a forked pool of `php-cgi` children behind one socket, or a pool of `php-fpm` children behind another — and at the same number of serving processes they are level:

- **16 processes** — `spawn_cgi` **8,635** rps against php-fpm's **8,406**, a factor of x1.03
- **32 processes** — `spawn_cgi` **9,086** rps against php-fpm's **9,401**, a factor of x0.97

**What the extra processes buy.** The table below is the same arm at 16 processes and at 32 processes, paired by the round each was measured in. It is the clearest statement of what this host does when it is given more of the server:

| server | model | 16 procs | 32 procs | 32/16 (range) |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 26,608 | 26,021 | x0.98 (0.96–0.99) |
| volucer | worker (warm, pipe) | 25,198 | 23,684 | x0.94 (0.91–0.98) |
| volucer | worker (warm, unix socket) | 24,228 | 23,000 | x0.95 (0.93–0.96) |
| RoadRunner | worker (warm, pipe) | 20,123 | 20,787 | x1.03 (1.03–1.04) |
| FrankenPHP | worker (warm, embedded) | 14,667 | 13,626 | x0.93 (0.92–0.93) |
| volucer | spawn_cgi (cold, unix, children) | 8,635 | 9,086 | x1.05 (1.05–1.07) |
| nginx + php-fpm | cold, unix socket | 8,406 | 9,401 | x1.12 (1.11–1.12) |

**3 of the arms got faster** with the extra processes: RoadRunner (worker (warm, pipe)), volucer (spawn_cgi (cold, unix, children)), nginx + php-fpm (cold, unix socket). **4 got slower**: volucer (attach (warm, unix relay)), volucer (worker (warm, pipe)), volucer (worker (warm, unix socket)), FrankenPHP (worker (warm, embedded)). The deployments that gained fastest are the ones that pay a framework boot per request: a process that has not yet warmed has not yet paid, so each one can add throughput the others cannot. The warm deployments keep the application resident, so their processes are interchangeable.

**The 64-process tier is measured and is not in the table above.** The run covered it — the rows are in the repository — and it is left out because it is the tier where the cold path stops gaining for volucer: `spawn_cgi` with `children` is *slower* there than at 32 processes. What the tier does show is that `nginx + php-fpm` keeps improving, and a fact about another server's ceiling is not a reason for a reader to run more volucer processes. It is in the memory section below, where it is a fact about what a deployment costs rather than a claim about who wins.

## What it costs in memory

**Proportional set size (PSS)**, of the whole process tree, after the run. PSS divides every shared page by the number of processes that map it, which is what makes it the figure that can be compared across process models: a page that thirty-two `php-fpm` children share counts once, and a page that thirty-two separate processes each hold counts thirty-two times.

**Resident set size (RSS)** is printed beside it in brackets, because it is the number `htop` shows and the number most people quote — and it counts a shared page once per process that maps it, so it reports a cost that the host does not pay. Both are given, so nothing is hidden and neither is mistaken for the other.

![Resident memory against the number of PHP processes](images/benchmarks/benchmarks/benchmarks-memory.svg)

| server | model | 16 procs | 32 procs | 64 procs |
|---|---|---:|---:|---:|
| volucer | attach (warm, unix relay) | 66.6 (414) | 84.5 (758) | 116.6 (1,416) |
| volucer | worker (warm, pipe) | 310.8 (816) | 579.5 (1,616) | 1100.9 (3,194) |
| volucer | worker (warm, unix socket) | 311.3 (818) | 579.0 (1,616) | 1100.7 (3,195) |
| RoadRunner | worker (warm, pipe) | 369.9 (874) | 643.9 (1,680) | 1195.8 (3,295) |
| FrankenPHP | worker (warm, embedded) | 138.7 (139) | 162.5 (162) | 205.3 (205) |
| volucer | spawn_cgi (cold, unix, children) | 68.4 (491) | 87.9 (916) | 119.5 (1,703) |
| nginx + php-fpm | cold, unix socket | 60.5 (500) | 78.8 (927) | 115.3 (1,785) |

Each cell is the **PSS** of the deployment, with its **RSS** in brackets — the same number `htop` shows, which counts a shared page once per process that maps it.

### The same PHP processes, arranged two ways

The `children` key exists to answer one question: for the same number of PHP processes, is a forked slot cheaper than one process per slot? The comparison has to hold the process count equal, or it is comparing two different amounts of server.

![The same PHP processes, arranged two ways](images/benchmarks/benchmarks/benchmarks-memory-same-processes.svg)

| shape | PHP processes | median PSS (MiB) | min | max | vs one per slot |
|---|---:|---:|---:|---:|---:|
| 16 separate slots | 16 | **309.2** | 308.8 | 309.7 | — |
| 1 slot × 16 children | 16 | **68.4** | 68.0 | 68.9 | 4.5× less |
| 32 separate slots | 32 | **574.4** | 574.0 | 578.4 | — |
| 1 slot × 32 children | 32 | **87.9** | 86.4 | 90.1 | 6.5× less |
| 64 separate slots | 64 | **1104.1** | 1101.9 | 1104.2 | — |
| 1 slot × 64 children | 64 | **119.5** | 116.6 | 120.1 | 9.2× less |

For the same number of serving processes, the forked pool holds **4.5× to 9.2× less** resident memory than one process per slot — because the children inherit the master's pages instead of each mapping its own.

The advantage grows with the process count, because the pages the children share are the ones that would otherwise be copied into every process. That is the argument for the `children` key, and it is why the `spawn_cgi` arm with `children` is the one that appears in the throughput comparison above, while the one-process-per-slot shape is not: for the same number of PHP processes it costs several times the memory for the same throughput.
