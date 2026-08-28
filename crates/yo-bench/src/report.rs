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
        !self.unresolved && self.ratio >= 10.0 && self.our_peak_kb <= self.best_rival_peak_kb
    }

    /// What the verdict column says.
    pub fn verdict(&self) -> &'static str {
        if self.unresolved {
            "unresolved"
        } else if self.passes() {
            "pass"
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

        let _ = writeln!(s, "## Against the best rival\n");
        let _ = writeln!(
            s,
            "The ratio is ours over the faster of Redis and Valkey on the same row, and the memory column is ours against the leaner of the two. Ten times and no worse on memory is a pass.\n"
        );
        let _ = writeln!(
            s,
            "A row marked unresolved is one redis-benchmark could not tell apart. It ends a run on a 250 millisecond timer tick and divides the request count by the clock it read there, so two servers that finished within a tick of each other come out on exactly the same number. The ratio on such a row is an artifact and means nothing. Runs are sized in a calibration pass to be long enough that this does not happen, so a row marked here is one where the sizing was overridden or the calibration was wrong.\n"
        );
        let _ = writeln!(
            s,
            "| command | generator | pipeline | yo ops/sec | best rival | rival ops/sec | ratio | yo peak MiB | rival peak MiB | verdict |"
        );
        let _ = writeln!(
            s,
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
        );
        let verdicts = self.verdicts();
        for v in &verdicts {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {:.2}x | {:.1} | {:.1} | {} |",
                v.case.op,
                v.case.driver,
                v.case.pipeline,
                thousands(v.ours),
                v.best_rival_name,
                thousands(v.best_rival),
                v.ratio,
                mib(v.our_peak_kb),
                mib(v.best_rival_peak_kb),
                v.verdict(),
            );
        }
        let _ = writeln!(s);

        let passed = verdicts.iter().filter(|v| v.passes()).count();
        let bound = verdicts.iter().filter(|v| v.unresolved).count();
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
                "{passed} of {} cases clear ten times, and the worst case that measured a server is {worst:.2}x.",
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

    #[test]
    fn a_case_with_no_rival_row_is_left_out_rather_than_scored() {
        let r = report(vec![row("yo", Kind::Yo, 1_000_000.0, 10_000)]);
        assert!(r.verdicts().is_empty());
    }
}
