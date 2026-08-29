//! What to run, against what, with which knobs.
//!
//! A plan is a value and not a config file. The knobs that matter here are a
//! dozen numbers, they change with a recompile that takes two seconds, and a
//! config file would buy a parser, a schema, an error path and a second place
//! for the truth to live. When a plan needs to come from somewhere else it can
//! be built by hand and handed to `run`.

use std::fmt;

/// One of the commands a gate is written against.
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
    /// Add one member to one set.
    ///
    /// The hot key row. Both generators send it against a single key for the
    /// whole run with a member drawn at random out of the keyspace, so every
    /// connection is contending for the same record and there is no spread to
    /// exploit. That is the shape aki came in at 0.82x on and the shape the M3
    /// gate is written against.
    ///
    /// The four commands below need a set that already has members in it, which
    /// is what [`Fixture`] is for.
    Sadd,
    /// Take a member out of a set and give it back.
    ///
    /// The one row where the run empties the thing it is measuring. Everything
    /// awkward about it follows from that: it cannot be calibrated by probing,
    /// because the probe would drain the set before the measured pass got to
    /// it, and it cannot run for ten seconds, because at two million a second
    /// ten seconds is twenty million pops and no fixture worth building is that
    /// big. So it runs a fixed count against a fixture sized above it, and the
    /// report carries the shorter elapsed time rather than pretending.
    Spop,
    /// Draw a member out of a set and leave it there.
    ///
    /// The same shape as SPOP with the awkward part removed. It touches one
    /// key, does one draw and changes nothing, so the fixture is built once and
    /// every pass sees the same set.
    Srandmember,
    /// Everything in both of two sets.
    Sinter,
    /// Everything in either of two sets.
    ///
    /// The reply is most of what this costs: two thousand member sets with a
    /// five hundred member overlap union to fifteen hundred members, so every
    /// command frames fifteen hundred bulk strings. That is what SUNION is, for
    /// us and for the rivals equally, and a row that cut the sets down until
    /// the reply stopped mattering would be measuring something nobody runs.
    Sunion,
}

/// A collection that has to exist before a command can be measured against it.
///
/// Members are named the way memtier names keys, `memtier-<n>` for n in the
/// range, so two fixtures overlap by however much their ranges overlap and by
/// nothing else. That is the whole reason the range is spelled out here instead
/// of a count: SINTER against two sets that happen to share nothing is a
/// benchmark of returning the empty array.
///
/// The fill is a random draw over the range rather than a walk across it,
/// because a walk has to come off one connection to stay in order and a draw
/// can use all of them. Coverage is 1 - e^(-n/k) and the builder sends five
/// times the range, so a fixture is about 99.3 percent of its nominal size.
/// Nothing here depends on the exact count and every server gets the same
/// treatment, so the fraction of a percent is left alone.
#[derive(Debug, Clone, Copy)]
pub struct Fixture {
    /// The key to build.
    pub key: &'static str,
    /// The lowest member id.
    pub from: u64,
    /// How many ids the range covers.
    pub members: u64,
}

impl Fixture {
    /// The highest member id.
    pub fn to(&self) -> u64 {
        self.from + self.members - 1
    }
}

/// Members in the set SPOP empties.
const POP_MEMBERS: u64 = 4_000_000;

/// Commands in a SPOP run.
///
/// Under the fixture with room to spare, because a run that reaches the bottom
/// stops being a SPOP benchmark at the moment it gets there and the rest of the
/// row is the server saying the set is empty at whatever rate it says that.
const POP_RUN: u64 = 3_000_000;

/// Members in the set SRANDMEMBER draws from.
const RAND_MEMBERS: u64 = 1_000_000;

/// Members in each of the two sets the algebra rows work on.
///
/// A thousand and not a million. SINTER over two million member sets is one
/// command a second, which is a real thing servers do and is not a throughput
/// row: the gate wants to know what set algebra costs per command against a
/// rival, and at a thousand members that question has an answer both ends of
/// the comparison can produce a few hundred thousand of.
const ALGEBRA_MEMBERS: u64 = 1_000;

/// How many of those two sets are the same member.
const ALGEBRA_SHARED: u64 = 500;

const POP_FIXTURE: [Fixture; 1] = [Fixture {
    key: "set:pop",
    from: 1,
    members: POP_MEMBERS,
}];

const RAND_FIXTURE: [Fixture; 1] = [Fixture {
    key: "set:rand",
    from: 1,
    members: RAND_MEMBERS,
}];

const ALGEBRA_FIXTURE: [Fixture; 2] = [
    Fixture {
        key: "set:a",
        from: 1,
        members: ALGEBRA_MEMBERS,
    },
    Fixture {
        key: "set:b",
        from: ALGEBRA_MEMBERS - ALGEBRA_SHARED + 1,
        members: ALGEBRA_MEMBERS,
    },
];

impl Op {
    /// The name `redis-benchmark -t` knows it by.
    pub fn bench_name(self) -> &'static str {
        match self {
            Op::Set => "set",
            Op::Get => "get",
            Op::Incr => "incr",
            Op::Mset => "mset",
            Op::Sadd => "sadd",
            Op::Spop => "spop",
            Op::Srandmember => "srandmember",
            Op::Sinter => "sinter",
            Op::Sunion => "sunion",
            // The multi bulk form and not the inline one, because inline PING
            // is a different number of bytes on the wire and no real client
            // sends it.
            Op::Ping => "ping_mbulk",
        }
    }

    /// What has to be in the database before this command means anything.
    ///
    /// Empty for most of them, because most of them build their own keys as
    /// they go. The set reads do not, and a SPOP against a key that is not
    /// there is a null reply at whatever rate the server can produce nulls.
    pub fn fixtures(self) -> &'static [Fixture] {
        match self {
            Op::Spop => &POP_FIXTURE,
            Op::Srandmember => &RAND_FIXTURE,
            Op::Sinter | Op::Sunion => &ALGEBRA_FIXTURE,
            _ => &[],
        }
    }

    /// How many commands a run is, when the fixture and not the clock decides.
    ///
    /// Only SPOP, and only because it consumes what it reads. Everything else
    /// is calibrated by probing until the run lasts [`Plan::min_seconds`], and
    /// probing a draining op would empty the set before the measured pass
    /// started.
    pub fn fixed_requests(self) -> Option<u64> {
        match self {
            Op::Spop => Some(POP_RUN),
            _ => None,
        }
    }

    /// Whether a run of this leaves the database in a state the next run cannot
    /// use.
    ///
    /// A draining op gets its fixture rebuilt before every pass, and gets no
    /// warmup pass at all, because the fixture build is millions of writes
    /// against the same server and is a better warmup than a tenth of the run
    /// would have been.
    pub fn drains(self) -> bool {
        self == Op::Spop
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Op::Set => "SET",
            Op::Get => "GET",
            Op::Incr => "INCR",
            Op::Mset => "MSET",
            Op::Sadd => "SADD",
            Op::Spop => "SPOP",
            Op::Srandmember => "SRANDMEMBER",
            Op::Sinter => "SINTER",
            Op::Sunion => "SUNION",
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
    ///
    /// It goes the other way for anything with a fixture. `redis-benchmark` has
    /// a `-t spop` and it is not usable here: it pops from a key its own `-t
    /// sadd` test fills, so run on its own it pops from a set that is not there
    /// and reports how fast the server can say so. Building the fixture with
    /// memtier and reading it with `redis-benchmark` is worse, because the two
    /// name members differently and it would be a hundred percent miss rate
    /// dressed up as a benchmark. So the fixture rows are memtier only, which
    /// is the same rule as MSET pointed the other way: a generator gets a row
    /// when it can set the row up as well as send it.
    pub fn can_run(self, op: Op) -> bool {
        match self {
            Driver::RedisBenchmark => op.fixtures().is_empty(),
            Driver::Memtier => op != Op::Mset,
        }
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
    /// The gate plan: nine commands, two generators, two pipeline depths.
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
            for op in [
                Op::Set,
                Op::Get,
                Op::Incr,
                Op::Mset,
                Op::Sadd,
                Op::Spop,
                Op::Srandmember,
                Op::Sinter,
                Op::Sunion,
            ] {
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
    ///
    /// A case whose op has a fixed count keeps it. That count is the one thing
    /// about the run that is not a tuning knob: it comes out of the size of the
    /// fixture, and raising it past the fixture would measure the empty set.
    pub fn size_cases(&mut self, requests: u64) {
        for c in &mut self.cases {
            c.requests = c.op.fixed_requests().unwrap_or(requests);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of spelling ranges out instead of counts. If these two sets
    /// stopped overlapping, SINTER would still run, still produce a number, and
    /// that number would be the rate at which a server returns the empty array.
    /// It is the failure that looks most like a result.
    #[test]
    fn the_two_algebra_sets_share_exactly_what_they_are_meant_to() {
        let [a, b] = ALGEBRA_FIXTURE;
        let shared = a.to().min(b.to()).saturating_sub(a.from.max(b.from)) + 1;
        assert_eq!(shared, ALGEBRA_SHARED);
        assert_ne!(a.key, b.key);
        assert!(shared < a.members, "an overlap, not one set twice");
    }

    /// The one invariant SPOP has. A run longer than the fixture reaches the
    /// bottom partway through and spends the rest of itself measuring how fast
    /// the server says the set is empty, which is faster than a pop and drags
    /// the row upward.
    #[test]
    fn a_pop_run_cannot_reach_the_bottom_of_its_fixture() {
        let run = Op::Spop.fixed_requests().expect("SPOP has a fixed count");
        let held: u64 = Op::Spop.fixtures().iter().map(|f| f.members).sum();
        // Fixtures are filled by random draw and land about 99.3 percent full,
        // so the margin has to be more than a rounding one.
        assert!(run < held * 9 / 10, "{run} pops out of {held} members");
    }

    /// A generator gets a row when it can set the row up as well as send it.
    #[test]
    fn only_memtier_drives_the_rows_that_need_a_fixture() {
        for op in [Op::Spop, Op::Srandmember, Op::Sinter, Op::Sunion] {
            assert!(!op.fixtures().is_empty(), "{op} is a fixture row");
            assert!(!Driver::RedisBenchmark.can_run(op), "{op}");
            assert!(Driver::Memtier.can_run(op), "{op}");
        }
        // And the rule did not quietly take the old rows out with it.
        for op in [Op::Set, Op::Get, Op::Incr, Op::Sadd, Op::Ping] {
            assert!(Driver::RedisBenchmark.can_run(op), "{op}");
            assert!(Driver::Memtier.can_run(op), "{op}");
        }
        assert!(Driver::RedisBenchmark.can_run(Op::Mset));
        assert!(!Driver::Memtier.can_run(Op::Mset));
    }

    /// Calibration is for ops whose run length is a question. SPOP's is not,
    /// and a calibration pass that raised it would walk it past the fixture.
    #[test]
    fn sizing_leaves_the_fixed_counts_alone() {
        let mut plan = Plan::gate(Vec::new(), "redis-benchmark".into(), "memtier".into());
        plan.size_cases(50_000_000);
        for c in &plan.cases {
            match c.op.fixed_requests() {
                Some(n) => assert_eq!(c.requests, n, "{}", c.op),
                None => assert_eq!(c.requests, 50_000_000, "{}", c.op),
            }
        }
    }
}
