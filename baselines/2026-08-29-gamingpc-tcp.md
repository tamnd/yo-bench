# The first full gate run with a measured ceiling, gamingpc, 2026-08-29

This is the run the ceiling rule was written for, and it is the first one where every wire row is scored against a number that was measured on the same box in the same session rather than against a bar that turned out to be unreachable over a socket.

Three things are worth reading off it before the tables.

At pipeline 1 we set the ceiling ourselves on both generators, 199,787 a second on redis-benchmark and 207,657 on memtier, against Redis at 163,087 and 175,521 and Valkey at 177,595 and 185,242. PING does no work at all, so this says our wire path from the read to the reply is the fastest of the three on this box, by 13 to 22 percent.

At pipeline 1 the real commands sit close behind it. SET on redis-benchmark is at 98 percent of the ceiling, GET at 87 and 91, INCR at 90 and 93. That is what the bar was set at, 85 percent, and four of the seven pipeline 1 rows clear it outright.

At pipeline 16 nothing clears it, ours or theirs. The best rival row on that depth is Redis GET at 73 percent of the ceiling, and the reason is not the transport: at depth the syscall is amortised over sixteen commands and what is left is the per command work, which PING does not do. The bar stays where it is because it is reachable in principle, GET at 85 percent of 2,648,316 means 68 nanoseconds of marginal cost against 36.7 in process, but the failing rows at depth are a list of work to do rather than a broken rule.

The one row that is a genuine anomaly is GET on memtier at pipeline 16, 1,361,540 with a p99 of 4.6 milliseconds where the same command on redis-benchmark ran at 1,988,127 with a p99 of 695 microseconds. Both rivals are near 2 million on that row. That is ours to explain.

Everything below is the report as the harness wrote it, unedited.

## yo-bench gate

GamingPC, Linux 6.18.33.2-microsoft-standard-WSL2, 13th Gen Intel(R) Core(TM) i9-13900K (32 cores), 31 GiB

48 connections over 4 generator threads, 1000000 commands per measured run, 64 byte values, 1000000 keys, best of 3.

### Under test

- yo: yo 0.3.1 (io threads 1)
- redis: Redis server v=8.10.1 sha=00000000:1 malloc=jemalloc-5.3.0 bits=64 build=96a8831ea991e869 (io threads 1)
- valkey: Valkey server v=9.1.1 sha=00000000:1 malloc=jemalloc-5.3.0 bits=64 build=4cb3ada3e0e9fbd0 (io threads 1)

### Every row

| command | generator | pipeline | server | ops/sec | seconds | p50 us | p99 us | RSS MiB | peak MiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| PING | redis-benchmark | 1 | yo | 199,787 | 10.0 | 207 | 535 | 2.9 | 2.9 |
| PING | redis-benchmark | 1 | redis | 163,087 | 12.3 | 255 | 615 | 9.4 | 9.4 |
| PING | redis-benchmark | 1 | valkey | 177,595 | 11.3 | 239 | 583 | 8.6 | 8.7 |
| PING | memtier | 1 | yo | 207,657 | 9.0 | 215 | 543 | 2.9 | 2.9 |
| PING | memtier | 1 | redis | 175,521 | 10.0 | 255 | 599 | 9.5 | 9.5 |
| PING | memtier | 1 | valkey | 185,242 | 10.0 | 247 | 567 | 8.6 | 8.6 |
| PING | redis-benchmark | 16 | yo | 2,562,578 | 7.8 | 263 | 623 | 3.0 | 3.0 |
| PING | redis-benchmark | 16 | redis | 2,648,316 | 7.5 | 255 | 551 | 9.1 | 9.1 |
| PING | redis-benchmark | 16 | valkey | 2,206,783 | 9.0 | 311 | 703 | 8.6 | 8.6 |
| PING | memtier | 16 | yo | 3,195,559 | 4.0 | 223 | 543 | 3.0 | 3.0 |
| PING | memtier | 16 | redis | 2,618,060 | 4.0 | 279 | 615 | 9.5 | 9.5 |
| PING | memtier | 16 | valkey | 2,210,410 | 5.0 | 335 | 679 | 8.8 | 8.8 |
| SET | redis-benchmark | 1 | yo | 195,875 | 8.5 | 207 | 519 | 115.0 | 115.0 |
| SET | redis-benchmark | 1 | redis | 162,443 | 10.3 | 263 | 623 | 137.6 | 137.6 |
| SET | redis-benchmark | 1 | valkey | 175,282 | 9.5 | 239 | 535 | 120.3 | 120.3 |
| SET | memtier | 1 | yo | 165,574 | 11.0 | 271 | 663 | 92.6 | 92.6 |
| SET | memtier | 1 | redis | 173,607 | 11.0 | 255 | 639 | 114.3 | 116.9 |
| SET | memtier | 1 | valkey | 177,494 | 11.0 | 255 | 591 | 94.3 | 94.3 |
| GET | redis-benchmark | 1 | yo | 172,824 | 9.3 | 239 | 575 | 115.6 | 115.6 |
| GET | redis-benchmark | 1 | redis | 177,623 | 9.0 | 239 | 567 | 138.0 | 141.6 |
| GET | redis-benchmark | 1 | valkey | 155,951 | 10.3 | 271 | 655 | 120.3 | 120.3 |
| GET | memtier | 1 | yo | 189,724 | 9.0 | 223 | 743 | 115.7 | 115.7 |
| GET | memtier | 1 | redis | 170,054 | 9.0 | 271 | 591 | 137.1 | 140.9 |
| GET | memtier | 1 | valkey | 169,161 | 9.0 | 271 | 591 | 120.3 | 120.3 |
| INCR | redis-benchmark | 1 | yo | 185,658 | 10.3 | 223 | 591 | 69.3 | 69.3 |
| INCR | redis-benchmark | 1 | redis | 158,561 | 12.0 | 263 | 615 | 61.4 | 61.4 |
| INCR | redis-benchmark | 1 | valkey | 165,467 | 11.5 | 255 | 591 | 66.7 | 66.7 |
| INCR | memtier | 1 | yo | 185,879 | 10.0 | 239 | 607 | 42.9 | 42.9 |
| INCR | memtier | 1 | redis | 154,119 | 12.0 | 295 | 679 | 51.9 | 52.5 |
| INCR | memtier | 1 | valkey | 161,516 | 12.0 | 287 | 639 | 49.8 | 49.8 |
| MSET | redis-benchmark | 1 | yo | 154,315 | 9.3 | 279 | 639 | 115.7 | 115.7 |
| MSET | redis-benchmark | 1 | redis | 126,864 | 11.3 | 335 | 759 | 138.2 | 140.9 |
| MSET | redis-benchmark | 1 | valkey | 158,601 | 9.0 | 279 | 583 | 120.9 | 120.9 |
| SET | redis-benchmark | 16 | yo | 1,516,410 | 8.8 | 471 | 871 | 115.9 | 115.9 |
| SET | redis-benchmark | 16 | redis | 1,474,470 | 9.0 | 479 | 807 | 138.5 | 141.9 |
| SET | redis-benchmark | 16 | valkey | 1,294,583 | 10.3 | 551 | 983 | 120.8 | 120.8 |
| SET | memtier | 16 | yo | 1,932,408 | 6.0 | 367 | 951 | 115.8 | 115.8 |
| SET | memtier | 16 | redis | 1,476,236 | 7.0 | 511 | 927 | 137.6 | 140.9 |
| SET | memtier | 16 | valkey | 1,466,340 | 7.0 | 495 | 991 | 121.1 | 121.1 |
| GET | redis-benchmark | 16 | yo | 1,988,127 | 10.0 | 351 | 695 | 115.6 | 115.6 |
| GET | redis-benchmark | 16 | redis | 1,939,650 | 10.3 | 359 | 695 | 137.6 | 141.6 |
| GET | redis-benchmark | 16 | valkey | 1,691,811 | 11.8 | 415 | 823 | 120.3 | 120.3 |
| GET | memtier | 16 | yo | 1,361,540 | 8.0 | 383 | 4639 | 116.7 | 116.7 |
| GET | memtier | 16 | redis | 2,006,663 | 6.0 | 367 | 703 | 136.9 | 138.2 |
| GET | memtier | 16 | valkey | 2,072,422 | 5.0 | 359 | 695 | 120.4 | 120.4 |
| INCR | redis-benchmark | 16 | yo | 2,149,997 | 9.3 | 319 | 639 | 69.9 | 69.9 |
| INCR | redis-benchmark | 16 | redis | 1,988,795 | 10.0 | 351 | 679 | 61.4 | 65.1 |
| INCR | redis-benchmark | 16 | valkey | 1,988,597 | 10.0 | 351 | 703 | 66.6 | 66.6 |
| INCR | memtier | 16 | yo | 2,211,163 | 5.0 | 335 | 655 | 54.5 | 54.5 |
| INCR | memtier | 16 | redis | 1,930,724 | 6.0 | 383 | 767 | 60.7 | 63.6 |
| INCR | memtier | 16 | valkey | 2,026,886 | 6.0 | 367 | 679 | 66.2 | 66.2 |
| MSET | redis-benchmark | 16 | yo | 302,109 | 12.0 | 2183 | 5215 | 116.3 | 116.3 |
| MSET | redis-benchmark | 16 | redis | 321,970 | 11.3 | 2239 | 3367 | 138.4 | 142.1 |
| MSET | redis-benchmark | 16 | valkey | 212,915 | 17.0 | 2407 | 14151 | 120.7 | 120.7 |

### The ceiling

PING reads no key, allocates nothing and frames a four byte reply, so whatever it runs at is the fastest anything can answer a client on this box over this transport. It is measured for every server under test and not just for ours, and the ceiling is the fastest of them. The fastest and not the average, because a number one server demonstrated is a number the box can do, and because our own PING bounds every other row we run whatever the rivals did, PING being the same server doing strictly less work.

| generator | pipeline | server | ops/sec |
| --- | --- | --- | --- |
| redis-benchmark | 1 | yo | 199,787 |
| redis-benchmark | 1 | redis | 163,087 |
| redis-benchmark | 1 | valkey | 177,595 |
| memtier | 1 | yo | 207,657 |
| memtier | 1 | redis | 175,521 |
| memtier | 1 | valkey | 185,242 |
| redis-benchmark | 16 | yo | 2,562,578 |
| redis-benchmark | 16 | redis | 2,648,316 |
| redis-benchmark | 16 | valkey | 2,206,783 |
| memtier | 16 | yo | 3,195,559 |
| memtier | 16 | redis | 2,618,060 |
| memtier | 16 | valkey | 2,210,410 |

The redis-benchmark ceiling at pipeline 1 is 199,787 per second, set by yo, so a row on that generator and depth passes at 169,819.
The servers are 18 percent apart there, which is more than the 15 percent that would say the number belongs to the box rather than to a server. It is yo's wire path that set it, and every row on that generator and depth is held to it.
The memtier ceiling at pipeline 1 is 207,657 per second, set by yo, so a row on that generator and depth passes at 176,509.
The servers are 15 percent apart there, which is more than the 15 percent that would say the number belongs to the box rather than to a server. It is yo's wire path that set it, and every row on that generator and depth is held to it.
The redis-benchmark ceiling at pipeline 16 is 2,648,316 per second, set by redis, so a row on that generator and depth passes at 2,251,068.
The servers are 17 percent apart there, which is more than the 15 percent that would say the number belongs to the box rather than to a server. It is redis's wire path that set it, and every row on that generator and depth is held to it.
The memtier ceiling at pipeline 16 is 3,195,559 per second, set by yo, so a row on that generator and depth passes at 2,716,225.
The servers are 31 percent apart there, which is more than the 15 percent that would say the number belongs to the box rather than to a server. It is yo's wire path that set it, and every row on that generator and depth is held to it.

### Against the best rival

The ratio is ours over the faster of Redis and Valkey on the same row, and the memory column is ours against the leaner of the two. The share column is ours over the PING ceiling for the same generator at the same pipeline depth, and where there is one it is the verdict: a row passes at 85 percent of the ceiling and no worse on memory. Where there is no ceiling the old bar applies, ten times the best rival and no worse on memory.

A row marked unresolved is one redis-benchmark could not tell apart. It ends a run on a 250 millisecond timer tick and divides the request count by the clock it read there, so two servers that finished within a tick of each other come out on exactly the same number. The ratio on such a row is an artifact and means nothing. Runs are sized in a calibration pass to be long enough that this does not happen, so a row marked here is one where the sizing was overridden or the calibration was wrong. Only rows scored on the ratio can be unresolved, because the ceiling bar does not compare two servers.

| command | generator | pipeline | yo ops/sec | best rival | rival ops/sec | ratio | share | yo peak MiB | rival peak MiB | verdict |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| SET | redis-benchmark | 1 | 195,875 | valkey | 175,282 | 1.12x | 98% | 115.0 | 120.3 | pass |
| SET | memtier | 1 | 165,574 | valkey | 177,494 | 0.93x | 80% | 92.6 | 94.3 | fail |
| GET | redis-benchmark | 1 | 172,824 | redis | 177,623 | 0.97x | 87% | 115.6 | 120.3 | pass |
| GET | memtier | 1 | 189,724 | redis | 170,054 | 1.12x | 91% | 115.7 | 120.3 | pass |
| INCR | redis-benchmark | 1 | 185,658 | valkey | 165,467 | 1.12x | 93% | 69.3 | 61.4 | fail |
| INCR | memtier | 1 | 185,879 | valkey | 161,516 | 1.15x | 90% | 42.9 | 49.8 | pass |
| MSET | redis-benchmark | 1 | 154,315 | valkey | 158,601 | 0.97x | 77% | 115.7 | 120.9 | fail |
| SET | redis-benchmark | 16 | 1,516,410 | redis | 1,474,470 | 1.03x | 57% | 115.9 | 120.8 | fail |
| SET | memtier | 16 | 1,932,408 | redis | 1,476,236 | 1.31x | 60% | 115.8 | 121.1 | fail |
| GET | redis-benchmark | 16 | 1,988,127 | redis | 1,939,650 | 1.02x | 75% | 115.6 | 120.3 | fail |
| GET | memtier | 16 | 1,361,540 | valkey | 2,072,422 | 0.66x | 43% | 116.7 | 120.4 | fail |
| INCR | redis-benchmark | 16 | 2,149,997 | redis | 1,988,795 | 1.08x | 81% | 69.9 | 65.1 | fail |
| INCR | memtier | 16 | 2,211,163 | valkey | 2,026,886 | 1.09x | 69% | 54.5 | 63.6 | fail |
| MSET | redis-benchmark | 16 | 302,109 | redis | 321,970 | 0.94x | 11% | 116.3 | 120.7 | fail |

### Where that leaves the gate

4 of 14 cases pass, 14 of them against the ceiling and the rest against the ratio. The worst ratio on a row that measured a server is 0.66x.

### The command lines

    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t ping_mbulk -n 1998474 -c 48 -P 1 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/memtier_benchmark-2.5.1 -s 127.0.0.1 -p 7411 -P redis -t 4 -c 12 -n 34681 --pipeline=1 -d 64 --key-minimum=1 --key-maximum=1000000 --hide-histogram --distinct-client-seed --command=PING
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t ping_mbulk -n 19867662 -c 48 -P 16 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/memtier_benchmark-2.5.1 -s 127.0.0.1 -p 7411 -P redis -t 4 -c 12 -n 207137 --pipeline=16 -d 64 --key-minimum=1 --key-maximum=1000000 --hide-histogram --distinct-client-seed --command=PING
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t set -n 1665527 -c 48 -P 1 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/memtier_benchmark-2.5.1 -s 127.0.0.1 -p 7411 -P redis -t 4 -c 12 -n 34682 --pipeline=1 -d 64 --key-minimum=1 --key-maximum=1000000 --hide-histogram --distinct-client-seed --key-pattern=R:R --ratio=1:0
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t get -n 1598963 -c 48 -P 1 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/memtier_benchmark-2.5.1 -s 127.0.0.1 -p 7411 -P redis -t 4 -c 12 -n 29733 --pipeline=1 -d 64 --key-minimum=1 --key-maximum=1000000 --hide-histogram --distinct-client-seed --key-pattern=R:R --ratio=0:1
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t incr -n 1903370 -c 48 -P 1 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/memtier_benchmark-2.5.1 -s 127.0.0.1 -p 7411 -P redis -t 4 -c 12 -n 34677 --pipeline=1 -d 64 --key-minimum=1 --key-maximum=1000000 --hide-histogram --distinct-client-seed --command=INCR __key__ --command-key-pattern=R
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t mset -n 1427724 -c 48 -P 1 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t set -n 13274641 -c 48 -P 16 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/memtier_benchmark-2.5.1 -s 127.0.0.1 -p 7411 -P redis -t 4 -c 12 -n 207049 --pipeline=16 -d 64 --key-minimum=1 --key-maximum=1000000 --hide-histogram --distinct-client-seed --key-pattern=R:R --ratio=1:0
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t get -n 19887217 -c 48 -P 16 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/memtier_benchmark-2.5.1 -s 127.0.0.1 -p 7411 -P redis -t 4 -c 12 -n 207109 --pipeline=16 -d 64 --key-minimum=1 --key-maximum=1000000 --hide-histogram --distinct-client-seed --key-pattern=R:R --ratio=0:1
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t incr -n 19893909 -c 48 -P 16 -d 64 -r 1000000 --threads 4 --csv
    /opt/yo-bench/bin/memtier_benchmark-2.5.1 -s 127.0.0.1 -p 7411 -P redis -t 4 -c 12 -n 206950 --pipeline=16 -d 64 --key-minimum=1 --key-maximum=1000000 --hide-histogram --distinct-client-seed --command=INCR __key__ --command-key-pattern=R
    /opt/yo-bench/bin/redis-benchmark-8.10.1 -h 127.0.0.1 -p 7411 -t mset -n 3628922 -c 48 -P 16 -d 64 -r 1000000 --threads 4 --csv
