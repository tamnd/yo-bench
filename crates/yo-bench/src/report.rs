//! Turning rows into a table, a JSON file and a verdict.
//!
//! The rule the whole repository exists to enforce is in `ratio`: our number
//! divided by the best rival number, not the average and not the worse of the
//! two. A claim of ten times against the slower of two rivals is a claim about
//! the slower of two rivals.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::machine::Machine;
use crate::plan::{Case, Driver, Kind, Op, Plan};

/// One server on one case.
#[derive(Debug, Clone)]
pub struct Row {
    /// Target name.
    pub target: String,
    /// Target family.
    pub kind: Kind,
    /// Version string reported by the binary.
    pub version: String,
    /// The command.
    pub op: Op,
    /// The generator.
    pub driver: Driver,
    /// Commands in flight per connection.
    pub pipeline: u32,
    /// Best commands per second across the repeats.
    pub ops: f64,
    /// Median round trip in microseconds, from the run that produced `ops`.
    pub p50_us: f64,
    /// Ninety ninth percentile, same run.
    pub p99_us: f64,
    /// How long that run took on our clock, in seconds.
    pub seconds: f64,
    /// Resident set at the end of the run, in kibibytes.
    pub rss_kb: u64,
    /// Highest resident set the kernel saw, in kibibytes.
    pub peak_kb: u64,
    /// The generator command line, so the row can be reproduced by hand.
    pub cmdline: String,
}

/// The smallest difference in run length `redis-benchmark` can see, in seconds.
///
/// It stops on a timer: `redis-benchmark.c` defines `SHOW_THROUGHPUT_INTERVAL`
/// as 250 milliseconds and `showThroughput` is the only thing in the file that
/// calls `aeStop`, so a run ends at the first tick at or after the last reply
/// and `config.totlatency` is the real elapsed time rounded up to a multiple of
/// a quarter of a second. The rate in the csv is the request count over that.
///
/// So two servers whose runs finish within one tick of each other report the
/// same throughput to the digit, and it is not a coincidence when they do. On
/// the first gate run, at a million commands and pipeline 16, yo and Redis and
/// Valkey all came out at exactly 1,331,558 on SET and exactly 1,996,008 on
/// GET, which is a million over 0.751 seconds and a million over 0.501. Every
/// number that plan produced divides out to a multiple of 250 milliseconds plus
/// a millisecond or two. Rerunning the same three servers at four million
/// commands gave 1.66M, 1.31M and 1.45M.
///
/// memtier does its own timing and does not do this, so the check below only
/// applies to the rows redis-benchmark drove.
const TICK: f64 = 0.25;

/// The share of the measured `PING` ceiling a wire row has to reach.
///
/// `bench/00` section 4.2. A command sent over a socket cannot be ten times
/// faster than a correct rival on the same box, because both of them pay for
/// the same round trip and the round trip is nearly all of it. Measured on the
/// gate box on 2026-08-29, `PING` at pipeline 1 ran between 142,716 and 185,864
/// across the three servers depending on the server and the hour, on a command
/// that reads no key and allocates nothing. Ten times Redis on the same day is
/// about 1.6 million, which is ten times faster than the fastest empty round
/// trip the box has ever done, so the ratio bar was not a hard target on these
/// rows, it was an unreachable one.
///
/// So a wire row is scored against the ceiling instead, the same way `bench/00`
/// section 4.1 already scores a bandwidth bound command against `memcpy`. The
/// ratio against the rivals stays in the table as context.
///
/// Eighty five percent, which is the number section 4.1 uses, kept the same
/// here rather than picked again.
const CEILING_SHARE: f64 = 0.85;

/// How far apart the servers' `PING` numbers can be before the report says so.
///
/// Not a gate. The ceiling is the fastest `PING` anyone managed either way,
/// because that is the hardest available bar and because a number that one
/// server demonstrated is a number the box can do. The spread is reported
/// because it says what kind of ceiling it is: servers within a few percent of
/// each other means the number belongs to the box and the transport, and
/// servers far apart means one of them has a wire path the others do not and
/// the bar is that one's number.
///
/// Both have been seen on the gate box within a day. The first measurement had
/// yo at 147,967, Redis at 158,372 and Valkey at 148,110 at pipeline 1, which
/// is seven percent. The first full run with the ceiling pass in it had yo at
/// 185,864, Redis at 159,850 and Valkey at 142,716 on the same generator and
/// depth, which is thirty percent with ours on top.
const CEILING_AGREEMENT: f64 = 0.15;

/// Per command cost with the transport subtracted, in nanoseconds.
fn work_ns(ops: f64, ping: Option<f64>) -> Option<f64> {
    let ping = ping?;
    if ops <= 0.0 || ping <= 0.0 || ops >= ping {
        return None;
    }
    Some(1e9 / ops - 1e9 / ping)
}

/// A key that identifies a case across targets.
type Key = (String, String, u32);

fn key(op: Op, driver: Driver, pipeline: u32) -> Key {
    (op.to_string(), driver.to_string(), pipeline)
}

/// What we beat the best rival by, on one case.
pub struct Verdict {
    /// The case.
    pub case: Case,
    /// Our throughput.
    pub ours: f64,
    /// The best rival throughput, which is the one the ratio is against.
    pub best_rival: f64,
    /// Which rival that was.
    pub best_rival_name: String,
    /// Throughput ratio.
    pub ratio: f64,
    /// Our peak resident set.
    pub our_peak_kb: u64,
    /// The best rival's peak resident set, meaning the smallest one.
    pub best_rival_peak_kb: u64,
    /// Whether the generator could tell these servers apart at all.
    ///
    /// True when `redis-benchmark` drove the row and our run and a rival's
    /// finished within one of its timer ticks of each other, which is the
    /// condition under which it reports both of them as the same number.
    pub unresolved: bool,
    /// The fastest `PING` any server managed on this generator and depth.
    ///
    /// `None` only when the plan did not measure one, in which case this row
    /// falls back to the ratio bar.
    pub ceiling: Option<f64>,
    /// What our own server answered `PING` at on this generator and depth.
    pub our_ping: Option<f64>,
    /// What the rival on this row answered `PING` at.
    pub rival_ping: Option<f64>,
}

impl Verdict {
    /// Whether this case clears the bar.
    ///
    /// Ten times on throughput and no worse on memory. A row that wins on
    /// throughput and loses on memory is a fail, which is written down in the
    /// milestone and is the reason this is one function and not two columns
    /// the reader is left to combine.
    ///
    /// An unresolved row does not pass, and that is the right answer rather
    /// than a special case: nothing was demonstrated on it. It is reported as
    /// its own thing rather than as a failure, because a row the generator
    /// could not resolve is not evidence that we are slow any more than it is
    /// evidence that we are fast.
    pub fn passes(&self) -> bool {
        if self.our_peak_kb > self.best_rival_peak_kb {
            return false;
        }
        match self.ceiling {
            // A row with a ceiling on it is scored against the ceiling and the
            // ratio is context. Nothing about the tick matters here: the bar is
            // a share of a number measured on the same generator at the same
            // depth, so both sides carry the same rounding.
            Some(c) => self.ours >= CEILING_SHARE * c,
            None => !self.unresolved && self.ratio >= 10.0,
        }
    }

    /// How much of the ceiling this row reached, if there is one.
    pub fn share(&self) -> Option<f64> {
        self.ceiling.filter(|c| *c > 0.0).map(|c| self.ours / c)
    }

    /// What the command itself cost us, in nanoseconds, with the transport
    /// taken out.
    ///
    /// A row runs at some number of commands a second and `PING` on the same
    /// server, generator and depth runs at another. `PING` reads no key,
    /// allocates nothing and frames four bytes, so the difference between the
    /// two per command times is what this server spent on the command and not
    /// on getting it off the wire and the answer back onto it.
    ///
    /// This is context and not a bar. At depth 1 the transport is most of the
    /// time and this number is small and noisy. At depth 16 the transport is
    /// amortised over sixteen commands and this is most of what is left, which
    /// is why a depth 16 row can be well clear of every rival and still sit at
    /// sixty percent of the ceiling: the ceiling is not doing the work.
    ///
    /// `None` when there is no `PING` to subtract, or when the row came out
    /// faster than `PING` did, which is measurement noise rather than a command
    /// that costs less than nothing.
    pub fn our_work_ns(&self) -> Option<f64> {
        work_ns(self.ours, self.our_ping)
    }

    /// The same number for the rival this row is compared against.
    pub fn rival_work_ns(&self) -> Option<f64> {
        work_ns(self.best_rival, self.rival_ping)
    }

    /// How many times cheaper the command is on our side, transport removed.
    ///
    /// Above one means we spend less per command than the rival does. This is
    /// the closest thing the wire has to the in process number, and it is
    /// reported rather than gated, because what a client actually gets is the
    /// throughput column and not this one.
    pub fn work_ratio(&self) -> Option<f64> {
        match (self.our_work_ns(), self.rival_work_ns()) {
            (Some(ours), Some(theirs)) if ours > 0.0 => Some(theirs / ours),
            _ => None,
        }
    }

    /// What the verdict column says.
    pub fn verdict(&self) -> &'static str {
        if self.passes() {
            "pass"
        } else if self.ceiling.is_none() && self.unresolved {
            "unresolved"
        } else {
            "fail"
        }
    }
}

/// Everything one invocation produced.
pub struct Report {
    /// The plan that was run.
    pub plan: Plan,
    /// The box it ran on.
    pub machine: Machine,
    /// Every row, in the order they were measured.
    pub rows: Vec<Row>,
}

impl Report {
    /// The fastest `PING` any server managed, per generator and pipeline depth.
    ///
    /// The fastest and not the average, because the bar has to be the hardest
    /// one the evidence supports. If a rival answered an empty command faster
    /// than we did, the box can do that and our number is short of what the box
    /// can do. If we answered fastest, our own `PING` is still a bound on every
    /// other row we run, since `PING` is the same server doing strictly less
    /// work. Either way the fastest of them is the number to beat.
    ///
    /// `None` only when nothing measured a `PING` on that generator and depth,
    /// which sends those rows back to the ratio bar.
    pub fn ceiling(&self, driver: Driver, pipeline: u32) -> Option<f64> {
        self.rows
            .iter()
            .filter(|r| r.op == Op::Ping && r.driver == driver && r.pipeline == pipeline)
            .map(|r| r.ops)
            .fold(None, |best: Option<f64>, ops| {
                Some(best.map_or(ops, |b| b.max(ops)))
            })
            .filter(|c| *c > 0.0)
    }

    /// How far apart the servers were on `PING`, as a share of the fastest.
    ///
    /// `None` when fewer than two servers were measured, because one server
    /// agrees with itself and that is not evidence of anything.
    pub fn ceiling_spread(&self, driver: Driver, pipeline: u32) -> Option<f64> {
        let pings: Vec<f64> = self
            .rows
            .iter()
            .filter(|r| r.op == Op::Ping && r.driver == driver && r.pipeline == pipeline)
            .map(|r| r.ops)
            .collect();
        if pings.len() < 2 {
            return None;
        }
        let hi = pings.iter().copied().fold(f64::MIN, f64::max);
        let lo = pings.iter().copied().fold(f64::MAX, f64::min);
        if hi <= 0.0 {
            return None;
        }
        Some((hi - lo) / hi)
    }

    /// What one named server answered `PING` at, on this generator and depth.
    ///
    /// The ceiling is the fastest of these and is what rows are scored against.
    /// This is a different question: it is the transport cost for that one
    /// server, and subtracting it from that same server's real rows leaves the
    /// cost of the command itself.
    pub fn ping_of(&self, target: &str, driver: Driver, pipeline: u32) -> Option<f64> {
        self.rows
            .iter()
            .find(|r| {
                r.op == Op::Ping
                    && r.driver == driver
                    && r.pipeline == pipeline
                    && r.target == target
            })
            .map(|r| r.ops)
            .filter(|ops| *ops > 0.0)
    }

    /// Which server set the ceiling on this generator and depth.
    fn ceiling_holder(&self, driver: Driver, pipeline: u32) -> Option<String> {
        self.rows
            .iter()
            .filter(|r| r.op == Op::Ping && r.driver == driver && r.pipeline == pipeline)
            .max_by(|a, b| a.ops.total_cmp(&b.ops))
            .map(|r| r.target.clone())
    }

    /// One verdict per case, in plan order.
    pub fn verdicts(&self) -> Vec<Verdict> {
        let mut ours: BTreeMap<Key, &Row> = BTreeMap::new();
        let mut rivals: BTreeMap<Key, Vec<&Row>> = BTreeMap::new();
        for row in &self.rows {
            let k = key(row.op, row.driver, row.pipeline);
            if row.kind.is_rival() {
                rivals.entry(k).or_default().push(row);
            } else {
                ours.insert(k, row);
            }
        }

        let mut out = Vec::new();
        for case in &self.plan.cases {
            // The ceiling is not a case. It is what the other cases are scored
            // against, and it goes in its own table.
            if case.op == Op::Ping {
                continue;
            }
            let k = key(case.op, case.driver, case.pipeline);
            let (Some(mine), Some(theirs)) = (ours.get(&k), rivals.get(&k)) else {
                continue;
            };
            let Some(best) = theirs.iter().max_by(|a, b| a.ops.total_cmp(&b.ops)) else {
                continue;
            };
            // The memory side takes the smallest rival footprint for the same
            // reason the throughput side takes the largest rival throughput.
            let leanest = theirs.iter().map(|r| r.peak_kb).min().unwrap_or(u64::MAX);
            // Every server on the row and not just the rivals. Two forks of the
            // same server landing close together says very little; a third
            // implementation landing on the same number as both of them says
            // the number came from the generator.
            // Against the nearest rival in run length and not the fastest one,
            // because the fastest one was picked using the very number that is
            // in question here.
            let nearest = theirs
                .iter()
                .map(|r| (mine.seconds - r.seconds).abs())
                .fold(f64::INFINITY, f64::min);
            let unresolved = case.driver == Driver::RedisBenchmark && nearest < TICK;
            out.push(Verdict {
                case: *case,
                ours: mine.ops,
                best_rival: best.ops,
                best_rival_name: best.target.clone(),
                ratio: if best.ops > 0.0 {
                    mine.ops / best.ops
                } else {
                    f64::NAN
                },
                our_peak_kb: mine.peak_kb,
                best_rival_peak_kb: leanest,
                unresolved,
                ceiling: self.ceiling(case.driver, case.pipeline),
                our_ping: self.ping_of(&mine.target, case.driver, case.pipeline),
                rival_ping: self.ping_of(&best.target, case.driver, case.pipeline),
            });
        }
        out
    }

    /// The markdown a human reads.
    pub fn markdown(&self) -> String {
        let mut s = String::new();
        let p = &self.plan;

        let _ = writeln!(s, "# yo-bench {}\n", p.name);
        let _ = writeln!(s, "{}\n", self.machine.summary());
        let _ = writeln!(
            s,
            "{} connections over {} generator threads, {} commands per measured run, {} byte values, {} keys, best of {}.\n",
            p.clients, p.threads, p.requests, p.value_bytes, p.keyspace, p.repeats
        );
        // The transport, said once and said plainly, because two reports that
        // differ only in this look identical at a glance and are not
        // comparable row for row.
        match &p.socket {
            Some(path) => {
                let _ = writeln!(
                    s,
                    "The load runs over the socket file {path}. Every server also listens on port {}, which is where the readiness check and the confound checks go, but no measured command went over it.\n",
                    p.port
                );
            }
            None => {
                let _ = writeln!(s, "The load runs over loopback TCP on port {}.\n", p.port);
            }
        }

        // The set rows are not sized the way every other row is sized, and a
        // reader who sees a two second run next to a ten second one deserves to
        // be told why here rather than have to work it out from the elapsed
        // column.
        if p.cases.iter().any(|c| !c.op.fixtures().is_empty()) {
            let mut said = std::collections::BTreeSet::new();
            let mut fixtures = Vec::new();
            for c in &p.cases {
                for f in c.op.fixtures() {
                    if said.insert(f.key) {
                        fixtures.push(format!("{} holds {} members", f.key, f.members));
                    }
                }
            }
            let _ = writeln!(
                s,
                "The set read rows run against keys built before the run rather than by it, and they are memtier rows only, because a generator gets a row here when it can set the row up as well as send it. The fixtures are: {}. Members are put in by random draw over a fixed range, so a fixture lands about 99.3 percent full and two fixtures share exactly as much as their ranges overlap.\n",
                fixtures.join(", ")
            );
            if let Some(c) = p
                .cases
                .iter()
                .find(|c| c.op.drains() && c.op.fixed_requests().is_some())
            {
                let _ = writeln!(
                    s,
                    "The {} rows are the exception to the ten second rule and run a fixed {} commands instead. The run consumes what it reads, so it cannot be calibrated by probing and it cannot be stretched: a run long enough to reach the bottom of the fixture would spend the rest of itself measuring how fast a server says the set is empty, which is faster than a pop and would drag the row upward. The fixture is rebuilt before every pass. Those rows are shorter than the others and the elapsed column says how much shorter, which is honest here because memtier reports its own elapsed time and does not round it to a quarter second the way redis-benchmark does.\n",
                    c.op, c.requests
                );
            }
        }

        let _ = writeln!(s, "## Under test\n");
        for t in &p.targets {
            let _ = writeln!(
                s,
                "- {}: {} (io threads {})",
                t.name, t.version, t.io_threads
            );
        }
        let _ = writeln!(s);

        let _ = writeln!(s, "## Every row\n");
        let _ = writeln!(
            s,
            "| command | generator | pipeline | server | ops/sec | seconds | p50 us | p99 us | RSS MiB | peak MiB |"
        );
        let _ = writeln!(
            s,
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
        );
        for r in &self.rows {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {:.1} | {:.0} | {:.0} | {:.1} | {:.1} |",
                r.op,
                r.driver,
                r.pipeline,
                r.target,
                thousands(r.ops),
                r.seconds,
                r.p50_us,
                r.p99_us,
                mib(r.rss_kb),
                mib(r.peak_kb),
            );
        }
        let _ = writeln!(s);

        self.ceiling_section(&mut s);

        let _ = writeln!(s, "## Against the best rival\n");
        let _ = writeln!(
            s,
            "The ratio is ours over the faster of Redis and Valkey on the same row, and the memory column is ours against the leaner of the two. The share column is ours over the PING ceiling for the same generator at the same pipeline depth, and where there is one it is the verdict: a row passes at {:.0} percent of the ceiling and no worse on memory. Where there is no ceiling the old bar applies, ten times the best rival and no worse on memory.\n",
            CEILING_SHARE * 100.0
        );
        let _ = writeln!(
            s,
            "A row marked unresolved is one redis-benchmark could not tell apart. It ends a run on a 250 millisecond timer tick and divides the request count by the clock it read there, so two servers that finished within a tick of each other come out on exactly the same number. The ratio on such a row is an artifact and means nothing. Runs are sized in a calibration pass to be long enough that this does not happen, so a row marked here is one where the sizing was overridden or the calibration was wrong. Only rows scored on the ratio can be unresolved, because the ceiling bar does not compare two servers.\n"
        );
        let _ = writeln!(
            s,
            "The two work columns are the same rows with the transport taken out: ours over this row minus ours over our own PING on the same generator and depth, in nanoseconds, and the same subtraction on the rival's side against its own PING. That is what the command cost the server once getting it off the wire is paid for. It is reported and not gated, because what a client gets is the throughput column, but it is the number that says whether a row that missed the ceiling missed it on the engine or on the socket.\n"
        );
        let _ = writeln!(
            s,
            "| command | generator | pipeline | yo ops/sec | best rival | rival ops/sec | ratio | share | yo work ns | rival work ns | yo peak MiB | rival peak MiB | verdict |"
        );
        let _ = writeln!(
            s,
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
        );
        let verdicts = self.verdicts();
        for v in &verdicts {
            let share = match v.share() {
                Some(x) => format!("{:.0}%", x * 100.0),
                None => "-".to_string(),
            };
            let ns = |x: Option<f64>| match x {
                Some(x) => format!("{x:.0}"),
                None => "-".to_string(),
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {:.2}x | {} | {} | {} | {:.1} | {:.1} | {} |",
                v.case.op,
                v.case.driver,
                v.case.pipeline,
                thousands(v.ours),
                v.best_rival_name,
                thousands(v.best_rival),
                v.ratio,
                share,
                ns(v.our_work_ns()),
                ns(v.rival_work_ns()),
                mib(v.our_peak_kb),
                mib(v.best_rival_peak_kb),
                v.verdict(),
            );
        }
        let _ = writeln!(s);

        let passed = verdicts.iter().filter(|v| v.passes()).count();
        let bound = verdicts
            .iter()
            .filter(|v| v.ceiling.is_none() && v.unresolved)
            .count();
        let on_ceiling = verdicts.iter().filter(|v| v.ceiling.is_some()).count();
        // The worst ratio is over the rows that measured something. Including
        // an unresolved row would put a 1.00x in the summary line that no
        // server earned.
        let worst = verdicts
            .iter()
            .filter(|v| !v.unresolved)
            .map(|v| v.ratio)
            .fold(f64::INFINITY, f64::min);
        let _ = writeln!(s, "## Where that leaves the gate\n");
        if verdicts.is_empty() {
            let _ = writeln!(
                s,
                "No case had both a yo row and a rival row, so there is nothing to compare."
            );
        } else {
            let _ = writeln!(
                s,
                "{passed} of {} cases pass, {on_ceiling} of them against the ceiling and the rest against the ratio. The worst ratio on a row that measured a server is {worst:.2}x.",
                verdicts.len()
            );
            if bound > 0 {
                let _ = writeln!(
                    s,
                    "\n{bound} of them were too close for redis-benchmark to resolve and are not a result for anybody."
                );
            }
            // The same rows read a second way. A run can miss the ceiling on
            // every depth 16 row and still be spending less per command than
            // anything else on the box, and a reader who only has the share
            // column cannot tell that from being slow.
            let worked: Vec<f64> = verdicts.iter().filter_map(|v| v.work_ratio()).collect();
            if !worked.is_empty() {
                let cheaper = worked.iter().filter(|r| **r > 1.0).count();
                let lo = worked.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = worked.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let _ = writeln!(
                    s,
                    "\nWith the transport subtracted, the command itself is cheaper on our side on {cheaper} of the {} rows where both sides had a PING to subtract, over a range of {lo:.2}x to {hi:.2}x.",
                    worked.len()
                );
            }
        }
        let _ = writeln!(s);

        let _ = writeln!(s, "## The command lines\n");
        let mut seen = Vec::new();
        for r in &self.rows {
            if !seen.contains(&r.cmdline) {
                seen.push(r.cmdline.clone());
            }
        }
        for line in &seen {
            let _ = writeln!(s, "    {line}");
        }
        s
    }

    /// The `PING` table and what it means, written into the report.
    ///
    /// This goes above the verdict table on purpose. It is the number every row
    /// below it is judged against, and a reader who sees the ratios first will
    /// read them as a result.
    fn ceiling_section(&self, s: &mut String) {
        let pings: Vec<&Row> = self.rows.iter().filter(|r| r.op == Op::Ping).collect();
        if pings.is_empty() {
            return;
        }
        let _ = writeln!(s, "## The ceiling\n");
        let _ = writeln!(
            s,
            "PING reads no key, allocates nothing and frames a four byte reply, so whatever it runs at is the fastest anything can answer a client on this box over this transport. It is measured for every server under test and not just for ours, and the ceiling is the fastest of them. The fastest and not the average, because a number one server demonstrated is a number the box can do, and because our own PING bounds every other row we run whatever the rivals did, PING being the same server doing strictly less work.\n"
        );
        let _ = writeln!(s, "| generator | pipeline | server | ops/sec |");
        let _ = writeln!(s, "| --- | --- | --- | --- |");
        for r in &pings {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} |",
                r.driver,
                r.pipeline,
                r.target,
                thousands(r.ops)
            );
        }
        let _ = writeln!(s);

        let mut seen: Vec<(Driver, u32)> = Vec::new();
        for r in &pings {
            if !seen.contains(&(r.driver, r.pipeline)) {
                seen.push((r.driver, r.pipeline));
            }
        }
        for (driver, pipeline) in seen {
            let Some(c) = self.ceiling(driver, pipeline) else {
                continue;
            };
            let holder = self.ceiling_holder(driver, pipeline).unwrap_or_default();
            let _ = writeln!(
                s,
                "The {driver} ceiling at pipeline {pipeline} is {} per second, set by {holder}, so a row on that generator and depth passes at {}.",
                thousands(c),
                thousands(c * CEILING_SHARE)
            );
            match self.ceiling_spread(driver, pipeline) {
                Some(spread) if spread > CEILING_AGREEMENT => {
                    let _ = writeln!(
                        s,
                        "The servers are {:.0} percent apart there, which is more than the {:.0} percent that would say the number belongs to the box rather than to a server. It is {holder}'s wire path that set it, and every row on that generator and depth is held to it.",
                        spread * 100.0,
                        CEILING_AGREEMENT * 100.0
                    );
                }
                Some(spread) => {
                    let _ = writeln!(
                        s,
                        "The servers are within {:.0} percent of each other there, which is the evidence that the number is the box and the transport rather than any one of them.",
                        spread * 100.0
                    );
                }
                None => {
                    let _ = writeln!(
                        s,
                        "Only one server was measured there, so the number is ours and not the box's, and it still bounds every other row on that generator and depth."
                    );
                }
            }
        }
        let _ = writeln!(s);
    }

    /// The JSON another program reads.
    ///
    /// Written by hand because everything in it is a number or a string with
    /// no quotes in it, and a serialisation crate would be the only dependency
    /// in the tree.
    pub fn json(&self) -> String {
        let mut s = String::new();
        s.push_str("{\n");
        let _ = writeln!(s, "  \"plan\": \"{}\",", self.plan.name);
        let _ = writeln!(s, "  \"host\": \"{}\",", self.machine.host);
        let _ = writeln!(s, "  \"kernel\": \"{}\",", self.machine.kernel);
        let _ = writeln!(s, "  \"cpu\": \"{}\",", esc(&self.machine.cpu));
        let _ = writeln!(s, "  \"cores\": {},", self.machine.cores);
        let _ = writeln!(s, "  \"clients\": {},", self.plan.clients);
        let _ = writeln!(s, "  \"threads\": {},", self.plan.threads);
        let _ = writeln!(s, "  \"requests\": {},", self.plan.requests);
        let _ = writeln!(s, "  \"value_bytes\": {},", self.plan.value_bytes);
        let _ = writeln!(s, "  \"keyspace\": {},", self.plan.keyspace);
        let _ = writeln!(
            s,
            "  \"transport\": \"{}\",",
            match &self.plan.socket {
                Some(path) => format!("unix:{}", esc(path)),
                None => "tcp".to_string(),
            }
        );
        // The bar the wire rows were judged against, so a reader of the JSON
        // does not have to rederive it from the PING rows.
        s.push_str("  \"ceiling\": [");
        let mut ceilings = Vec::new();
        for r in self.rows.iter().filter(|r| r.op == Op::Ping) {
            let k = (r.driver, r.pipeline);
            if !ceilings.iter().any(|(d, p): &(Driver, u32)| (*d, *p) == k) {
                ceilings.push(k);
            }
        }
        for (i, (driver, pipeline)) in ceilings.iter().enumerate() {
            let comma = if i + 1 == ceilings.len() { "" } else { "," };
            let ops = match self.ceiling(*driver, *pipeline) {
                Some(c) => format!("{c:.2}"),
                None => "null".to_string(),
            };
            let _ = write!(
                s,
                "\n    {{\"generator\": \"{driver}\", \"pipeline\": {pipeline}, \"ops\": {ops}}}{comma}"
            );
        }
        s.push_str(if ceilings.is_empty() {
            "],\n"
        } else {
            "\n  ],\n"
        });
        s.push_str("  \"rows\": [\n");
        for (i, r) in self.rows.iter().enumerate() {
            let comma = if i + 1 == self.rows.len() { "" } else { "," };
            let _ = writeln!(
                s,
                "    {{\"target\": \"{}\", \"version\": \"{}\", \"op\": \"{}\", \"generator\": \"{}\", \"pipeline\": {}, \"ops\": {:.2}, \"seconds\": {:.3}, \"p50_us\": {:.2}, \"p99_us\": {:.2}, \"rss_kb\": {}, \"peak_kb\": {}}}{comma}",
                r.target,
                esc(&r.version),
                r.op,
                r.driver,
                r.pipeline,
                r.ops,
                r.seconds,
                r.p50_us,
                r.p99_us,
                r.rss_kb,
                r.peak_kb,
            );
        }
        s.push_str("  ]\n}\n");
        s
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn mib(kb: u64) -> f64 {
    kb as f64 / 1024.0
}

/// `1234567.8` as `1,234,568`, because a table of ten digit numbers with no
/// separators is a table nobody reads correctly.
fn thousands(v: f64) -> String {
    let n = v.round() as i64;
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_of_three_from_the_right() {
        assert_eq!(thousands(1.0), "1");
        assert_eq!(thousands(999.4), "999");
        assert_eq!(thousands(1000.0), "1,000");
        assert_eq!(thousands(1_234_567.8), "1,234,568");
    }

    /// How many commands the rows below pretend to have run.
    ///
    /// The helper derives the run length from the rate so that the two agree,
    /// which is what the tick check reads. Ten million at these rates puts the
    /// runs between seven and a hundred seconds, which is the range the gate
    /// calibrates into.
    const N: f64 = 10_000_000.0;

    fn row(target: &str, kind: Kind, ops: f64, peak_kb: u64) -> Row {
        rb_row(target, kind, ops, peak_kb, Driver::RedisBenchmark)
    }

    fn rb_row(target: &str, kind: Kind, ops: f64, peak_kb: u64, driver: Driver) -> Row {
        Row {
            target: target.to_string(),
            kind,
            version: "test".to_string(),
            op: Op::Set,
            driver,
            pipeline: 1,
            ops,
            p50_us: 1.0,
            p99_us: 2.0,
            seconds: N / ops,
            rss_kb: peak_kb,
            peak_kb,
            cmdline: String::new(),
        }
    }

    fn report(rows: Vec<Row>) -> Report {
        let driver = rows.first().map_or(Driver::RedisBenchmark, |r| r.driver);
        let plan = Plan::smoke(Vec::new(), "rb".into(), "mt".into());
        let mut plan = plan;
        plan.cases = vec![Case {
            op: Op::Set,
            driver,
            pipeline: 1,
            requests: N as u64,
        }];
        Report {
            plan,
            machine: Machine::probe(),
            rows,
        }
    }

    #[test]
    fn the_ratio_is_against_the_faster_rival_and_not_the_slower_one() {
        let r = report(vec![
            row("yo", Kind::Yo, 1_000_000.0, 10_000),
            row("redis", Kind::Redis, 100_000.0, 40_000),
            row("valkey", Kind::Valkey, 50_000.0, 40_000),
        ]);
        let v = r.verdicts();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].best_rival_name, "redis");
        assert!((v[0].ratio - 10.0).abs() < 1e-9);
        assert!(v[0].passes());
    }

    #[test]
    fn winning_on_throughput_and_losing_on_memory_is_a_fail() {
        let r = report(vec![
            row("yo", Kind::Yo, 1_000_000.0, 100_000),
            row("redis", Kind::Redis, 50_000.0, 40_000),
        ]);
        let v = r.verdicts();
        assert!(v[0].ratio > 10.0);
        assert!(
            !v[0].passes(),
            "20x on throughput and 2.5x the memory is not a pass"
        );
    }

    /// Three servers reporting the same number is the timer and not them.
    ///
    /// These are the values redis-benchmark actually printed for yo, Redis and
    /// Valkey on the SET pipeline 16 row of the first gate run on gamingpc. The
    /// row went into the report as 1.00x, which reads as parity with Redis and
    /// is not what happened.
    #[test]
    fn three_servers_on_the_same_number_is_the_timer() {
        let r = report(vec![
            row("yo", Kind::Yo, 1_331_558.0, 129_000),
            row("redis", Kind::Redis, 1_331_558.0, 119_000),
            row("valkey", Kind::Valkey, 1_331_558.0, 119_000),
        ]);
        let v = r.verdicts();
        assert!(v[0].unresolved);
        assert!(!v[0].passes(), "nothing was demonstrated on this row");
        assert_eq!(v[0].verdict(), "unresolved");
    }

    /// A row where the servers came out apart is a result, even a close one.
    ///
    /// Redis and Valkey are a fork of each other and land near each other all
    /// the time, so the check has to be tight enough that a real row with a
    /// small spread still counts.
    #[test]
    fn a_close_row_is_still_a_result() {
        let r = report(vec![
            row("yo", Kind::Yo, 150_795.0, 84_000),
            row("redis", Kind::Redis, 170_003.0, 77_000),
            row("valkey", Kind::Valkey, 169_000.0, 78_000),
        ]);
        let v = r.verdicts();
        assert!(!v[0].unresolved, "an 11 percent spread is a measurement");
        assert_eq!(v[0].verdict(), "fail");
    }

    /// One rival is enough, because the tick is not a coincidence.
    ///
    /// The earlier version of this check wanted two rivals agreeing before it
    /// would call a row an artifact, on the reasoning that one close pair is
    /// just a close pair. That reasoning was about spread in the rates. The
    /// timer is a property of the generator, so a single rival that finished
    /// within a tick of us is already a row that cannot say which of us was
    /// faster.
    #[test]
    fn one_rival_inside_a_tick_is_enough() {
        let r = report(vec![
            row("yo", Kind::Yo, 1_331_558.0, 129_000),
            row("redis", Kind::Redis, 1_331_558.0, 119_000),
        ]);
        assert!(r.verdicts()[0].unresolved);
    }

    /// memtier times itself and is not subject to any of this.
    ///
    /// Its numbers were never quantised, so a close memtier row is a close
    /// memtier row and gets scored like any other.
    #[test]
    fn a_memtier_row_is_never_marked_unresolved() {
        let r = report(vec![
            rb_row("yo", Kind::Yo, 1_331_558.0, 129_000, Driver::Memtier),
            rb_row("redis", Kind::Redis, 1_331_558.0, 119_000, Driver::Memtier),
        ]);
        let v = r.verdicts();
        assert!(!v[0].unresolved);
        assert_eq!(v[0].verdict(), "fail");
    }

    fn ping(target: &str, kind: Kind, ops: f64) -> Row {
        let mut r = rb_row(target, kind, ops, 10_000, Driver::RedisBenchmark);
        r.op = Op::Ping;
        r
    }

    /// Add the ceiling to a report the way a real session would.
    ///
    /// The measured pass writes a `PING` row per server and the plan carries a
    /// `PING` case, so both halves are put in here rather than just the rows.
    fn with_ceiling(mut r: Report, pings: Vec<Row>) -> Report {
        r.plan.cases.insert(
            0,
            Case {
                op: Op::Ping,
                driver: Driver::RedisBenchmark,
                pipeline: 1,
                requests: N as u64,
            },
        );
        for p in pings {
            r.rows.push(p);
        }
        r
    }

    /// The numbers from the gate box on 2026-08-29, at pipeline 1.
    fn real_pings() -> Vec<Row> {
        vec![
            ping("yo", Kind::Yo, 147_967.0),
            ping("redis", Kind::Redis, 158_372.0),
            ping("valkey", Kind::Valkey, 148_110.0),
        ]
    }

    /// The work column is this server's row against this server's own PING.
    ///
    /// Not against the ceiling. The ceiling belongs to whoever was fastest, and
    /// subtracting somebody else's transport from our row would put their
    /// socket in our number.
    #[test]
    fn the_work_column_subtracts_each_servers_own_transport() {
        // Depth 16 numbers off the gate box on 2026-08-29, where the ceiling
        // and the row are far enough apart for the subtraction to mean
        // something.
        let mut yo_get = rb_row("yo", Kind::Yo, 1_988_127.0, 118_000, Driver::RedisBenchmark);
        yo_get.pipeline = 16;
        let mut redis_get = rb_row(
            "redis",
            Kind::Redis,
            1_939_650.0,
            140_000,
            Driver::RedisBenchmark,
        );
        redis_get.pipeline = 16;
        let mut r = report(vec![yo_get, redis_get]);
        r.plan.cases[0].pipeline = 16;
        r.plan.cases.insert(
            0,
            Case {
                op: Op::Ping,
                driver: Driver::RedisBenchmark,
                pipeline: 16,
                requests: N as u64,
            },
        );
        for (target, kind, ops) in [
            ("yo", Kind::Yo, 2_562_578.0),
            ("redis", Kind::Redis, 2_648_316.0),
        ] {
            let mut p = ping(target, kind, ops);
            p.pipeline = 16;
            r.rows.push(p);
        }

        let v = &r.verdicts()[0];
        let ours = v.our_work_ns().expect("both rows are there");
        let theirs = v.rival_work_ns().expect("both rows are there");
        // 1e9/1988127 - 1e9/2562578 and 1e9/1939650 - 1e9/2648316.
        assert!((ours - 112.8).abs() < 0.5, "{ours}");
        assert!((theirs - 138.0).abs() < 0.5, "{theirs}");
        let ratio = v.work_ratio().expect("both sides");
        assert!((ratio - 1.22).abs() < 0.01, "{ratio}");
        // The share on that same row is 75 percent, which is the thing the work
        // column exists to put in context: a fail on the ceiling and a win on
        // the command.
        assert!(!v.passes());
    }

    /// A row that came out faster than the PING it would be measured against
    /// has nothing to say about cost, so it says nothing.
    #[test]
    fn a_command_never_costs_less_than_nothing() {
        assert_eq!(work_ns(200_000.0, Some(190_000.0)), None);
        assert_eq!(work_ns(200_000.0, None), None);
        assert!(work_ns(150_000.0, Some(200_000.0)).is_some());
    }

    /// The ceiling is the fastest empty round trip anybody managed.
    #[test]
    fn a_ceiling_is_the_fastest_ping_anyone_managed() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 140_000.0, 10_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            real_pings(),
        );
        let c = r
            .ceiling(Driver::RedisBenchmark, 1)
            .expect("three PING rows were measured");
        assert!((c - 158_372.0).abs() < 1e-9, "Redis was fastest that hour");
        let spread = r
            .ceiling_spread(Driver::RedisBenchmark, 1)
            .expect("three rows");
        assert!(spread < CEILING_AGREEMENT, "{spread}");
    }

    /// A row at nine tenths of the wire is a pass at 1.08x the rival.
    ///
    /// This is the whole point of the change. Ten times 130,000 is 1.3 million
    /// on a box where nothing can answer faster than 158,372, so the ratio bar
    /// failed a row that had taken almost everything there was to take.
    #[test]
    fn a_wire_row_is_scored_against_the_ceiling_and_not_the_rival() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 140_000.0, 10_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            real_pings(),
        );
        let v = r.verdicts();
        assert_eq!(v.len(), 1, "the PING case is not a workload row");
        assert!(v[0].ratio < 1.1);
        assert!(v[0].passes(), "88 percent of the ceiling is a pass");
        assert_eq!(v[0].verdict(), "pass");
    }

    #[test]
    fn a_wire_row_well_under_the_ceiling_still_fails() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 100_000.0, 10_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            real_pings(),
        );
        assert!(!r.verdicts()[0].passes(), "63 percent of the ceiling");
    }

    /// Memory still decides, however close to the wire the row got.
    #[test]
    fn the_ceiling_does_not_excuse_losing_on_memory() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 150_000.0, 100_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            real_pings(),
        );
        assert!(!r.verdicts()[0].passes());
    }

    /// Our own PING bounds our own rows, whatever the rivals did.
    ///
    /// These are the numbers from the first full run with the ceiling pass in
    /// it, where ours came out fastest by thirty percent. The bar is then our
    /// own empty round trip, which is the right answer: a GET cannot be faster
    /// than the same server answering PING, because PING is that server doing
    /// strictly less.
    #[test]
    fn the_fastest_server_sets_the_bar_even_when_it_is_ours() {
        let pings = vec![
            ping("yo", Kind::Yo, 185_864.0),
            ping("redis", Kind::Redis, 159_850.0),
            ping("valkey", Kind::Valkey, 142_716.0),
        ];
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 150_000.0, 10_000),
                row("redis", Kind::Redis, 140_000.0, 40_000),
            ]),
            pings,
        );
        let c = r
            .ceiling(Driver::RedisBenchmark, 1)
            .expect("ours is a ceiling too");
        assert!((c - 185_864.0).abs() < 1e-9);
        let spread = r
            .ceiling_spread(Driver::RedisBenchmark, 1)
            .expect("three rows");
        assert!(spread > CEILING_AGREEMENT, "thirty percent apart: {spread}");
        assert!(
            !r.verdicts()[0].passes(),
            "81 percent of our own PING is not 85"
        );
    }

    /// One server alone still bounds itself, and the report says it is alone.
    #[test]
    fn one_server_alone_is_still_a_bound_on_that_server() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 140_000.0, 10_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            vec![ping("yo", Kind::Yo, 147_967.0)],
        );
        let c = r
            .ceiling(Driver::RedisBenchmark, 1)
            .expect("ours was measured");
        assert!((c - 147_967.0).abs() < 1e-9);
        assert!(
            r.ceiling_spread(Driver::RedisBenchmark, 1).is_none(),
            "one server agreeing with itself is not a spread"
        );
        assert!(r.verdicts()[0].passes(), "95 percent of our own PING");
    }

    /// A ceiling is per generator and per depth, not one number for the run.
    #[test]
    fn a_ceiling_does_not_carry_across_generators_or_depths() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 140_000.0, 10_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            real_pings(),
        );
        assert!(r.ceiling(Driver::Memtier, 1).is_none());
        assert!(r.ceiling(Driver::RedisBenchmark, 16).is_none());
    }

    /// The tick label is about telling two servers apart, which the ceiling
    /// bar does not do, so a row scored on the ceiling is never unresolved.
    #[test]
    fn a_row_with_a_ceiling_is_never_reported_as_unresolved() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 140_000.0, 10_000),
                row("redis", Kind::Redis, 140_000.0, 40_000),
            ]),
            real_pings(),
        );
        let v = r.verdicts();
        assert!(v[0].unresolved, "the rates are identical");
        assert_eq!(v[0].verdict(), "pass");
    }

    #[test]
    fn a_case_with_no_rival_row_is_left_out_rather_than_scored() {
        let r = report(vec![row("yo", Kind::Yo, 1_000_000.0, 10_000)]);
        assert!(r.verdicts().is_empty());
    }
}
