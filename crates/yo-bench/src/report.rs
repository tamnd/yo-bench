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
    /// Resident set at the end of the run, in kibibytes.
    pub rss_kb: u64,
    /// Highest resident set the kernel saw, in kibibytes.
    pub peak_kb: u64,
    /// The generator command line, so the row can be reproduced by hand.
    pub cmdline: String,
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
}

impl Verdict {
    /// Whether this case clears the bar.
    ///
    /// Ten times on throughput and no worse on memory. A row that wins on
    /// throughput and loses on memory is a fail, which is written down in the
    /// milestone and is the reason this is one function and not two columns
    /// the reader is left to combine.
    pub fn passes(&self) -> bool {
        self.ratio >= 10.0 && self.our_peak_kb <= self.best_rival_peak_kb
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
            "| command | generator | pipeline | server | ops/sec | p50 us | p99 us | RSS MiB | peak MiB |"
        );
        let _ = writeln!(s, "| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
        for r in &self.rows {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {:.0} | {:.0} | {:.1} | {:.1} |",
                r.op,
                r.driver,
                r.pipeline,
                r.target,
                thousands(r.ops),
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
                if v.passes() { "pass" } else { "fail" },
            );
        }
        let _ = writeln!(s);

        let passed = verdicts.iter().filter(|v| v.passes()).count();
        let worst = verdicts
            .iter()
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
                "{passed} of {} cases clear ten times, and the worst case is {worst:.2}x.",
                verdicts.len()
            );
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
                "    {{\"target\": \"{}\", \"version\": \"{}\", \"op\": \"{}\", \"generator\": \"{}\", \"pipeline\": {}, \"ops\": {:.2}, \"p50_us\": {:.2}, \"p99_us\": {:.2}, \"rss_kb\": {}, \"peak_kb\": {}}}{comma}",
                r.target,
                esc(&r.version),
                r.op,
                r.driver,
                r.pipeline,
                r.ops,
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

    fn row(target: &str, kind: Kind, ops: f64, peak_kb: u64) -> Row {
        Row {
            target: target.to_string(),
            kind,
            version: "test".to_string(),
            op: Op::Set,
            driver: Driver::RedisBenchmark,
            pipeline: 1,
            ops,
            p50_us: 1.0,
            p99_us: 2.0,
            rss_kb: peak_kb,
            peak_kb,
            cmdline: String::new(),
        }
    }

    fn report(rows: Vec<Row>) -> Report {
        let plan = Plan::smoke(Vec::new(), "rb".into(), "mt".into());
        let mut plan = plan;
        plan.cases = vec![Case {
            op: Op::Set,
            driver: Driver::RedisBenchmark,
            pipeline: 1,
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

    #[test]
    fn a_case_with_no_rival_row_is_left_out_rather_than_scored() {
        let r = report(vec![row("yo", Kind::Yo, 1_000_000.0, 10_000)]);
        assert!(r.verdicts().is_empty());
    }
}
