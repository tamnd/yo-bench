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
/// gate box on 2026-08-29, `PING` at pipeline 1 ran at 147,967 for yo, 158,372
/// for Redis and 148,110 for Valkey. Three servers with three event loops,
/// three allocators and two languages, inside seven percent of each other on a
/// command that does no work. Ten times Redis is 1,583,719, which is ten times
/// faster than the wire, so the ratio bar was not a hard target on these rows,
/// it was an unreachable one.
///
/// So a wire row is scored against the ceiling instead, the same way `bench/00`
/// section 4.1 already scores a bandwidth bound command against `memcpy`. The
/// ratio against the rivals stays in the table as context.
///
/// Eighty five percent, which is the number section 4.1 uses, kept the same
/// here rather than picked again.
const CEILING_SHARE: f64 = 0.85;

/// How far apart the servers' `PING` numbers can be and still be a ceiling.
///
/// The claim that a number is the box's and not a server's rests on unrelated
/// servers agreeing about it. If they stop agreeing, one of them is doing
/// something the others are not and the row is a result again, so the ratio
/// bar applies to it. Fifteen percent is loose enough to absorb the run to run
/// spread this rig sees on a busy box and tight enough that a real difference
/// in the network path shows up as one.
const CEILING_AGREEMENT: f64 = 0.15;

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
    /// `None` when the plan did not measure one, or when the servers did not
    /// agree closely enough for it to be called a ceiling, in which case this
    /// row falls back to the ratio bar.
    pub ceiling: Option<f64>,
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
    /// `None` for a depth the plan did not measure a `PING` at, and `None` when
    /// the servers were more than [`CEILING_AGREEMENT`] apart on it, because
    /// then it is not a fact about the box. Both cases send the row back to the
    /// ratio bar rather than silently passing it.
    pub fn ceiling(&self, driver: Driver, pipeline: u32) -> Option<f64> {
        let pings: Vec<f64> = self
            .rows
            .iter()
            .filter(|r| r.op == Op::Ping && r.driver == driver && r.pipeline == pipeline)
            .map(|r| r.ops)
            .collect();
        // Two at the least, because one server agreeing with itself is not
        // evidence of anything.
        if pings.len() < 2 {
            return None;
        }
        let hi = pings.iter().copied().fold(f64::MIN, f64::max);
        let lo = pings.iter().copied().fold(f64::MAX, f64::min);
        if hi <= 0.0 || (hi - lo) / hi > CEILING_AGREEMENT {
            return None;
        }
        Some(hi)
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
            "| command | generator | pipeline | yo ops/sec | best rival | rival ops/sec | ratio | share | yo peak MiB | rival peak MiB | verdict |"
        );
        let _ = writeln!(
            s,
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
        );
        let verdicts = self.verdicts();
        for v in &verdicts {
            let share = match v.share() {
                Some(x) => format!("{:.0}%", x * 100.0),
                None => "-".to_string(),
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {:.2}x | {} | {:.1} | {:.1} | {} |",
                v.case.op,
                v.case.driver,
                v.case.pipeline,
                thousands(v.ours),
                v.best_rival_name,
                thousands(v.best_rival),
                v.ratio,
                share,
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
            "PING reads no key, allocates nothing and frames a four byte reply, so whatever it runs at is the fastest anything can answer a client on this box over this transport. It is measured here for every server under test, not just ours, because the point of the number is that it belongs to the box and not to a server. Three unrelated servers landing within a few percent of each other is the evidence for that. If they come apart by more than {:.0} percent the number is not a ceiling, this report says so, and the rows fall back to being scored against the rivals.\n",
            CEILING_AGREEMENT * 100.0
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
            match self.ceiling(driver, pipeline) {
                Some(c) => {
                    let _ = writeln!(
                        s,
                        "The {driver} ceiling at pipeline {pipeline} is {} per second, so a row on that generator and depth passes at {}.",
                        thousands(c),
                        thousands(c * CEILING_SHARE)
                    );
                }
                None => {
                    let _ = writeln!(
                        s,
                        "The servers did not agree on PING under {driver} at pipeline {pipeline}, so there is no ceiling at that generator and depth and those rows are scored against the rivals."
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

    /// Three servers inside seven percent is a fact about the box.
    #[test]
    fn a_ceiling_is_the_fastest_of_servers_that_agree() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 140_000.0, 10_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            real_pings(),
        );
        let c = r
            .ceiling(Driver::RedisBenchmark, 1)
            .expect("seven percent apart is a ceiling");
        assert!((c - 158_372.0).abs() < 1e-9);
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

    /// If the servers stop agreeing, the number was never a ceiling.
    #[test]
    fn servers_far_apart_on_ping_do_not_make_a_ceiling() {
        let pings = vec![
            ping("yo", Kind::Yo, 300_000.0),
            ping("redis", Kind::Redis, 158_372.0),
            ping("valkey", Kind::Valkey, 148_110.0),
        ];
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 140_000.0, 10_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            pings,
        );
        assert!(r.ceiling(Driver::RedisBenchmark, 1).is_none());
        let v = r.verdicts();
        assert!(v[0].ceiling.is_none());
        assert!(!v[0].passes(), "back to the ratio bar, and 1.08x fails it");
    }

    /// One server agreeing with itself is not evidence of anything.
    #[test]
    fn one_server_alone_is_not_a_ceiling() {
        let r = with_ceiling(
            report(vec![
                row("yo", Kind::Yo, 140_000.0, 10_000),
                row("redis", Kind::Redis, 130_000.0, 40_000),
            ]),
            vec![ping("yo", Kind::Yo, 147_967.0)],
        );
        assert!(r.ceiling(Driver::RedisBenchmark, 1).is_none());
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
