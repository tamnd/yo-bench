# yo-bench

Benchmarks for yo against Redis and Valkey, with the 10x gate. Every axis, a memory column on every row, and the losses published next to the wins.

Nothing in here is a microbenchmark of our own code. That lives in the yo repository under `cargo bench`, where it belongs, and it answers a different question. This repository runs the two load generators the rest of the world already argues about, `redis-benchmark` and `memtier_benchmark`, against the real servers over a real socket, and reports what they said.

## The rule

The ratio on every row is ours divided by the faster of Redis and Valkey on that same row, and the memory column is ours against the leaner of the two. Not the average of the rivals and not whichever one happened to be slower that day. A row that wins on throughput and loses on memory is a fail, which the harness enforces rather than leaving to the reader.

Ten times, under both generators, at pipeline 1 and at pipeline 16, on SET, GET, INCR and MSET. That is the M2 gate. If it lands under ten times, the bar was wrong and the spec gets amended with the measurement attached, rather than the claim being quietly reinterpreted.

## Running it

Everything runs on the box under test. A generator on one machine and a server on another measures the network between them, which is a real thing to measure and is not what this is for.

    suite/provision.sh                # builds the rivals from source, once
    cargo run --release -- smoke      # three cases, checks the rig works
    cargo run --release -- gate       # the real thing

`provision.sh` builds Redis 8.10.1, Valkey 9.1.1 and memtier_benchmark 2.5.1 from the upstream release tarballs with the upstream Makefiles, into `/opt/yo-bench`. It does not touch anything the distribution installed. Distribution packages are whatever the distribution felt like shipping, and on the box this was written against that meant Redis 8.8.0 and Valkey 7.2.12, which is a year old on one side and two major versions behind on the other. The allocator is left at each project's own default, which on Linux means jemalloc for both of them, because forcing libc malloc would make our memory column look better and would be measuring a Redis nobody runs.

The versions are pinned and overridable:

    REDIS_VERSION=8.10.1 VALKEY_VERSION=9.1.1 MEMTIER_VERSION=2.5.1 suite/provision.sh

## What the harness does and does not do

It restarts the server between every case. A GET case that inherits whatever the SET case before it left behind is measuring a different dataset than the one it asked for, and a server that has been up for twenty cases has an allocator in a state a fresh one is not in.

It fills the keyspace before a read case, using the same generator that is about to read it. The two generators name keys differently, `key:000000000042` against `memtier-42`, so filling with one and reading with the other is a hundred percent miss rate dressed up as a benchmark.

It throws away a warm up pass before every measured one, and takes the best of three measured passes rather than the mean. Everything that makes one pass slower than its neighbours is another tenant on the box, and averaging that in does not make the number more honest.

It reads memory out of the kernel, `VmHWM` and `VmRSS`, and not out of the server's own `INFO`. Redis and Valkey both report `used_memory`, and neither of them counts the allocator's slack, the client buffers or the code, all of which are memory the machine actually spent.

It prints every generator command line into the report, so a row nobody believes can be pasted into a shell and checked.

It does not give our server a flag the rivals do not get. If yo needs a tuning flag to be fast then that flag is the default or the number does not count.

## Options worth knowing

    --only yo                 run one target
    --pipeline 1              one depth instead of both
    --requests 200000         shorter runs while iterating
    --io-threads 4            give Redis and Valkey four io threads, which is the confound row
    --pin 0-3,4-7             server on one set of cores, generator on another
    --yodb path/to/yodb       measure a specific build
    --socket /tmp/yo.sock     run the load over a socket file instead of loopback TCP

Without `--socket` the load runs over loopback TCP, which is what every published row here is so far. With it, all three servers are started with `--unixsocket` on that path and both generators are pointed at the file, so a socket file run is a full run against the same plan and not a spot check. The port stays open on every server either way, because the readiness check, the C1 confound check and the shutdown all go over it, and the only thing that moves is the measured load. A socket run names itself `gate-socket` so its report lands in its own directory, the report says which transport it used in its header, and the JSON carries it as a `transport` field, since two reports that differ only in this look identical at a glance and are not comparable row for row.

Three of the nine confounds in `bench/00` section 5 are checked here before anything is measured, and all three refuse rather than warn. C1 asks every server, ours included, for `INFO replication` after it comes up and stops the run unless it says `role:master` and `connected_slaves:0`, because a rival left as a replica of the subject is doing the subject's writes too. C2 reads the cpu mask back out of the kernel for every server, runs the same `taskset` line the generator runs under and reads that mask back as well, and refuses a layout where the two halves share a core. C3 refuses to run `redis-benchmark` on fewer than four threads, since on one thread it tops out near 470,000 commands a second and turns every pipeline 16 row into a tie at its own ceiling.

The `--io-threads 4` run is a confound check and not the headline. We are one shard on one thread by design, so a run where the rivals get four cores and we get one is a comparison of core counts. It gets its own row and its own column rather than being folded into the ratio.

## Reading the output

Each run writes `results/<host>-<plan>-<stamp>/` with `report.md`, `run.json` and the server logs. `results/` is not in the repository. Measurements are outputs, they go in a release note or an issue comment where the machine and the date are attached to them, not into the tree where they rot.

The report leads with the machine, including whether it is a virtual machine on shared hardware. A number from a VM is a baseline and not a ceiling, and saying so on the row is cheaper than arguing about it later.
