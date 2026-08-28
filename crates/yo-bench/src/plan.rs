//! What to run, against what, with which knobs.
//!
//! A plan is a value and not a config file. The knobs that matter here are a
//! dozen numbers, they change with a recompile that takes two seconds, and a
//! config file would buy a parser, a schema, an error path and a second place
//! for the truth to live. When a plan needs to come from somewhere else it can
//! be built by hand and handed to `run`.

use std::fmt;

/// One of the four commands the M2 gate is written against.
///
/// The set is small because the gate is small. A milestone that ships hashes
/// adds hashes here, and the row it produces has the same shape as these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Write a key.
    Set,
    /// Read a key.
    Get,
    /// Read, add one, write, reply with the number.
    Incr,
    /// Reply, and touch nothing.
    ///
    /// Not a workload. This is the ceiling: `PING` allocates nothing, reads no
    /// key and frames a four byte reply, so whatever it runs at is the fastest
    /// anything can answer a client on this box over this transport. Every
    /// other row on the same generator and pipeline depth is measured against
    /// it. See [`Plan::gate`] and `bench/00` section 4.2.
    Ping,
    /// Write ten keys in one command.
    ///
    /// `redis-benchmark` counts the command and not the ten keys, so an MSET
    /// row is ten times fewer operations per second than a SET row doing the
    /// same amount of work. That is upstream's convention and it is kept
    /// rather than corrected, because a number that does not match what the
    /// reader gets when they run the tool themselves is worse than a number
    /// that needs a sentence of explanation.
    Mset,
}

impl Op {
    /// The name `redis-benchmark -t` knows it by.
    pub fn bench_name(self) -> &'static str {
        match self {
            Op::Set => "set",
            Op::Get => "get",
            Op::Incr => "incr",
            Op::Mset => "mset",
            // The multi bulk form and not the inline one, because inline PING
            // is a different number of bytes on the wire and no real client
            // sends it.
            Op::Ping => "ping_mbulk",
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Op::Set => "SET",
            Op::Get => "GET",
            Op::Incr => "INCR",
            Op::Mset => "MSET",
            Op::Ping => "PING",
        })
    }
}

/// Which load generator drove the run.
///
/// Two of them, because the gate says the ratio holds under both. They disagree
/// with each other by 10 to 30 percent on the same server, which is the whole
/// reason for taking the minimum across them rather than picking the flattering
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    /// `redis-benchmark`, which ships with Redis.
    RedisBenchmark,
    /// `memtier_benchmark`, which is what Redis Labs publish numbers with.
    Memtier,
}

impl fmt::Display for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Driver::RedisBenchmark => "redis-benchmark",
            Driver::Memtier => "memtier",
        })
    }
}

impl Driver {
    /// Whether this generator can drive this command.
    ///
    /// memtier drives arbitrary commands through `--command`, but its key
    /// placeholder expands to the same key everywhere in one command, so an
    /// MSET built that way writes one key ten times and is not an MSET. That
    /// row comes from `redis-benchmark` only, and the report says so.
    pub fn can_run(self, op: Op) -> bool {
        !(self == Driver::Memtier && op == Op::Mset)
    }
}

/// Which server is under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Ours.
    Yo,
    /// `redis-server`.
    Redis,
    /// `valkey-server`.
    Valkey,
}

impl Kind {
    /// Whether a row from this server counts as a rival row.
    pub fn is_rival(self) -> bool {
        self != Kind::Yo
    }
}

/// A server to start, and how.
#[derive(Debug, Clone)]
pub struct Target {
    /// Short name used in the tables. `yo`, `redis`, `valkey`.
    pub name: String,
    /// Which family it belongs to, which decides the command line.
    pub kind: Kind,
    /// The binary to run.
    pub bin: String,
    /// The version string it reported when the plan was built.
    pub version: String,
    /// How many io threads to give it.
    ///
    /// One by default for everyone. Redis and Valkey will use more, we cannot,
    /// and a comparison where one side gets four cores and the other gets one
    /// is a comparison of core counts. The four thread run is a separate row
    /// with its own column, which is the confound check and not the headline.
    pub io_threads: u32,
}

/// One measured combination.
#[derive(Debug, Clone, Copy)]
pub struct Case {
    /// The command.
    pub op: Op,
    /// The generator.
    pub driver: Driver,
    /// How many commands are in flight per connection.
    pub pipeline: u32,
    /// Commands in the measured run, once the calibration pass has sized it.
    ///
    /// Starts at [`Plan::requests`] and is raised, per case, until the run is
    /// expected to last [`Plan::min_seconds`]. A pipeline 16 GET run and a
    /// pipeline 1 MSET run are twenty times apart in throughput, so one command
    /// count for both means one of them is far too short.
    pub requests: u64,
}

/// Everything a run needs.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Shows up in the report and in the directory name.
    pub name: String,
    /// The servers, in the order they are started.
    pub targets: Vec<Target>,
    /// The combinations, in the order they are run.
    pub cases: Vec<Case>,
    /// Connections the generator opens.
    pub clients: u32,
    /// Generator threads. The load has to be able to saturate the server.
    pub threads: u32,
    /// Commands per measured run before the calibration pass raises it.
    pub requests: u64,
    /// How long a measured run has to last.
    ///
    /// `redis-benchmark` stops on a 250 millisecond timer tick and divides the
    /// request count by the wall clock it read there, so its reported rate is
    /// the count over a multiple of 250 milliseconds. Two servers whose real
    /// elapsed times differ by less than one tick come out on exactly the same
    /// number: `redis-benchmark.c` defines `SHOW_THROUGHPUT_INTERVAL` as 250 and
    /// `showThroughput` is the only thing that calls `aeStop`.
    ///
    /// That is what produced the first gate run's 1.00x rows. At a million
    /// commands and pipeline 16 the runs took about three quarters of a second,
    /// so one tick was a third of the run and yo, Redis and Valkey all reported
    /// exactly 1,331,558 on SET and exactly 1,996,008 on GET. Rerun at four
    /// million and the same three servers came out at 1.66M, 1.31M and 1.45M.
    ///
    /// Ten seconds puts one tick at two and a half percent, which is well under
    /// anything this rig is trying to resolve. It costs about an hour on the
    /// full gate plan, which is the price of the table meaning something.
    pub min_seconds: f64,
    /// Value size in bytes.
    pub value_bytes: u32,
    /// How many distinct keys the run touches.
    pub keyspace: u64,
    /// How many measured runs per case. The report keeps the best.
    pub repeats: u32,
    /// Whether to throw away a short run first.
    pub warmup: bool,
    /// Port the server under test listens on.
    pub port: u16,
    /// Socket file the generators talk over, when the run is over one.
    ///
    /// `None` is the loopback TCP run, which is what everything before this
    /// measured and what the published rows are. `Some` points every generator
    /// at a socket file instead, which takes the IP header, the TCP state
    /// machine and the checksum out of every round trip and is the cheapest way
    /// to move the ceiling `bench/00` section 4.2 is about.
    ///
    /// Every server still listens on the port as well, so the readiness check,
    /// the `INFO replication` confound check and the shutdown all keep working
    /// the same way for all three of them. Only the load moves.
    pub socket: Option<String>,
    /// Cores for the server, if pinning is on.
    pub server_cpus: Option<String>,
    /// Cores for the generator, if pinning is on.
    pub load_cpus: Option<String>,
    /// Path to `redis-benchmark`.
    pub redis_benchmark: String,
    /// Path to `memtier_benchmark`.
    pub memtier: String,
}

impl Plan {
    /// The M2 gate plan: four commands, two generators, two pipeline depths.
    ///
    /// The numbers are the ones the methodology fixes. 64 byte values because
    /// the default of 3 measures the protocol and nothing else. A keyspace of
    /// a million because a keyspace of one fits in L1 and answers a different
    /// question. Pipeline 1 is the latency shape and pipeline 16 is the
    /// throughput shape, and a claim that only holds at one of them is not a
    /// claim about the server.
    pub fn gate(targets: Vec<Target>, redis_benchmark: String, memtier: String) -> Plan {
        let mut cases = Vec::new();
        // The ceiling first, so that a report that died halfway still says what
        // the box could do at all.
        for pipeline in [1, 16] {
            for driver in [Driver::RedisBenchmark, Driver::Memtier] {
                cases.push(Case {
                    op: Op::Ping,
                    driver,
                    pipeline,
                    requests: 0,
                });
            }
        }
        for pipeline in [1, 16] {
            for op in [Op::Set, Op::Get, Op::Incr, Op::Mset] {
                for driver in [Driver::RedisBenchmark, Driver::Memtier] {
                    if driver.can_run(op) {
                        cases.push(Case {
                            op,
                            driver,
                            pipeline,
                            requests: 0,
                        });
                    }
                }
            }
        }
        let mut plan = Plan {
            name: "gate".to_string(),
            targets,
            cases,
            // Divisible by the thread count, because memtier counts
            // connections per thread and would otherwise quietly round the
            // load down while redis-benchmark used the number asked for.
            clients: 48,
            threads: 4,
            requests: 1_000_000,
            min_seconds: 10.0,
            value_bytes: 64,
            keyspace: 1_000_000,
            repeats: 3,
            warmup: true,
            port: 7411,
            socket: None,
            server_cpus: None,
            load_cpus: None,
            redis_benchmark,
            memtier,
        };
        plan.size_cases(plan.requests);
        plan
    }

    /// Give every case the same command count.
    ///
    /// The starting point, and also what a run that skips calibration gets. A
    /// zero here would be a plan that measures nothing, so it is set once when
    /// the plan is built rather than left to whoever calls it.
    pub fn size_cases(&mut self, requests: u64) {
        for c in &mut self.cases {
            c.requests = requests;
        }
    }

    /// A small plan for checking the rig works, not for publishing.
    pub fn smoke(targets: Vec<Target>, redis_benchmark: String, memtier: String) -> Plan {
        let mut plan = Plan::gate(targets, redis_benchmark, memtier);
        plan.name = "smoke".to_string();
        plan.cases = vec![
            Case {
                op: Op::Set,
                driver: Driver::RedisBenchmark,
                pipeline: 1,
                requests: 0,
            },
            Case {
                op: Op::Get,
                driver: Driver::RedisBenchmark,
                pipeline: 16,
                requests: 0,
            },
            Case {
                op: Op::Set,
                driver: Driver::Memtier,
                pipeline: 16,
                requests: 0,
            },
        ];
        plan.size_cases(100_000);
        plan.requests = 100_000;
        // A smoke run is for finding out that the rig works, and a rig that
        // works is one that produced a number at all. Sizing every case to ten
        // seconds would turn a two minute check into half an hour.
        plan.min_seconds = 0.0;
        plan.repeats = 1;
        plan.warmup = false;
        plan
    }
}
