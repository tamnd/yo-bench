//! Driving the two public generators and reading what they said.
//!
//! Neither generator is wrapped or reimplemented. They are the binaries their
//! own projects ship, run with a command line that is printed into the report,
//! so anyone who does not believe a row can paste the line and get their own
//! number. The only thing this module adds is the arithmetic that makes the two
//! of them comparable, because `redis-benchmark -n` is a total and
//! `memtier -n` is per connection, and getting that backwards is a four times
//! error that looks like a result.

use std::io;
use std::process::Command;

use crate::plan::{Driver, Op, Plan};

/// What one measured run produced.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Commands per second, as the generator counted them.
    pub ops: f64,
    /// Median round trip in microseconds.
    pub p50_us: f64,
    /// Ninety ninth percentile round trip in microseconds.
    pub p99_us: f64,
    /// The command line, for the report.
    pub cmdline: String,
}

/// Build and run one measured pass.
pub fn run(
    driver: Driver,
    op: Op,
    plan: &Plan,
    pipeline: u32,
    requests: u64,
    quiet: bool,
) -> io::Result<Sample> {
    let (prog, args) = match driver {
        Driver::RedisBenchmark => redis_benchmark_args(op, plan, pipeline, requests),
        Driver::Memtier => memtier_args(op, plan, pipeline, requests),
    };

    let mut cmd = match &plan.load_cpus {
        Some(cpus) => {
            let mut c = Command::new("taskset");
            c.arg("-c").arg(cpus).arg(&prog).args(&args);
            c
        }
        None => {
            let mut c = Command::new(&prog);
            c.args(&args);
            c
        }
    };

    let cmdline = format!("{prog} {}", args.join(" "));
    if !quiet {
        eprintln!("    {cmdline}");
    }

    let out = cmd.output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "{prog} exited {}: {}",
            out.status,
            stderr.trim()
        )));
    }

    let mut sample = match driver {
        Driver::RedisBenchmark => parse_redis_benchmark(&stdout, op),
        Driver::Memtier => parse_memtier(&stdout),
    }
    .map_err(|e| io::Error::other(format!("{prog}: {e}\n--- output ---\n{stdout}\n{stderr}")))?;
    sample.cmdline = cmdline;
    Ok(sample)
}

/// The generator's own command line for filling the keyspace before a read run.
///
/// A GET run against an empty keyspace measures how fast a server can say no,
/// which every server here is very good at and which nobody runs in production.
/// The fill has to come from the same generator as the measured run because the
/// two of them name keys differently: `redis-benchmark` writes
/// `key:000000000042` and memtier writes `memtier-42`, so filling with one and
/// reading with the other is a hundred percent miss rate dressed up as a
/// benchmark.
pub fn preload(driver: Driver, plan: &Plan, quiet: bool) -> io::Result<()> {
    // Random keys, so coverage is 1 - e^(-n/k). Five times the keyspace puts
    // that at 99.3 percent, and the last fraction of a percent costs more than
    // it is worth.
    let requests = plan.keyspace.saturating_mul(5);
    if !quiet {
        eprintln!("    filling {} keys", plan.keyspace);
    }
    run(driver, Op::Set, plan, 64, requests, quiet).map(|_| ())
}

fn redis_benchmark_args(
    op: Op,
    plan: &Plan,
    pipeline: u32,
    requests: u64,
) -> (String, Vec<String>) {
    let args = vec![
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        plan.port.to_string(),
        "-t".into(),
        op.bench_name().into(),
        "-n".into(),
        requests.to_string(),
        "-c".into(),
        plan.clients.to_string(),
        "-P".into(),
        pipeline.to_string(),
        "-d".into(),
        plan.value_bytes.to_string(),
        "-r".into(),
        plan.keyspace.to_string(),
        "--threads".into(),
        plan.threads.to_string(),
        "--csv".into(),
    ];
    (plan.redis_benchmark.clone(), args)
}

fn memtier_args(op: Op, plan: &Plan, pipeline: u32, requests: u64) -> (String, Vec<String>) {
    // `-c` is connections per thread here, not connections. The plan counts
    // connections, so this is where that gets divided, and `-n` is per
    // connection so the total gets divided by both.
    let per_thread = (plan.clients / plan.threads).max(1);
    let conns = per_thread * plan.threads;
    let per_conn = (requests / u64::from(conns)).max(1);

    let mut args = vec![
        "-s".into(),
        "127.0.0.1".into(),
        "-p".into(),
        plan.port.to_string(),
        "-P".into(),
        "redis".into(),
        "-t".into(),
        plan.threads.to_string(),
        "-c".into(),
        per_thread.to_string(),
        "-n".into(),
        per_conn.to_string(),
        format!("--pipeline={pipeline}"),
        "-d".into(),
        plan.value_bytes.to_string(),
        "--key-minimum=1".into(),
        format!("--key-maximum={}", plan.keyspace),
        "--hide-histogram".into(),
        "--distinct-client-seed".into(),
    ];
    // The key pattern goes in the arm and not in the list above, because
    // memtier takes it under two different names and refuses to be given both.
    // An arbitrary command has to say `--command-key-pattern`, and passing
    // `--key-pattern` as well is an error rather than a duplicate: "when using
    // arbitrary command, key pattern is configured with --command-key-pattern
    // option", followed by the whole usage message. That killed the gate run
    // on the first INCR case, an hour into it and after every SET and GET row
    // had already been measured.
    match op {
        Op::Set => {
            args.push("--key-pattern=R:R".into());
            args.push("--ratio=1:0".into());
        }
        Op::Get => {
            args.push("--key-pattern=R:R".into());
            args.push("--ratio=0:1".into());
        }
        Op::Incr => {
            args.push("--command=INCR __key__".into());
            args.push("--command-key-pattern=R".into());
        }
        // Filtered out in `Driver::can_run`, and unreachable rather than silently
        // measuring the wrong thing if that ever changes.
        Op::Mset => unreachable!("memtier does not drive MSET"),
    }
    (plan.memtier.clone(), args)
}

/// Read the one row `--csv` prints for the test we asked for.
fn parse_redis_benchmark(out: &str, op: Op) -> Result<Sample, String> {
    let want = op.to_string();
    for line in out.lines() {
        let line = line.trim();
        if !line.starts_with('"') {
            continue;
        }
        let fields: Vec<String> = line
            .split("\",\"")
            .map(|f| f.trim_matches('"').to_string())
            .collect();
        if fields.len() < 8 {
            continue;
        }
        // The test name is `SET` for most of them and `MSET (10 keys)` for
        // MSET, so this is a prefix match and not equality.
        if !fields[0].to_uppercase().starts_with(&want) {
            continue;
        }
        let num = |i: usize| -> Result<f64, String> {
            fields[i]
                .parse::<f64>()
                .map_err(|_| format!("field {i} is {:?}", fields[i]))
        };
        return Ok(Sample {
            ops: num(1)?,
            p50_us: num(4)? * 1000.0,
            p99_us: num(6)? * 1000.0,
            cmdline: String::new(),
        });
    }
    Err(format!("no csv row for {want}"))
}

/// Read the `Totals` line out of memtier's summary table.
///
/// The JSON output would be tidier and would cost a JSON parser. The table has
/// nine columns and has had nine columns for every 2.x release, so this checks
/// the count and complains loudly rather than reading column six of a table
/// that grew a column.
fn parse_memtier(out: &str) -> Result<Sample, String> {
    for line in out.lines() {
        let line = line.trim();
        if !line.starts_with("Totals") {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() != 9 {
            return Err(format!(
                "Totals has {} columns, expected 9: {line:?}",
                f.len()
            ));
        }
        let num = |i: usize| -> Result<f64, String> {
            f[i].parse::<f64>()
                .map_err(|_| format!("column {i} is {:?}", f[i]))
        };
        // Type, Ops/sec, Hits/sec, Misses/sec, Avg Latency, p50, p99, p99.9, KB/sec
        return Ok(Sample {
            ops: num(1)?,
            p50_us: num(5)? * 1000.0,
            p99_us: num(6)? * 1000.0,
            cmdline: String::new(),
        });
    }
    Err("no Totals line".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redis_benchmark_row_is_read_off_the_csv() {
        let out = "\"test\",\"rps\",\"avg_latency_ms\",\"min_latency_ms\",\"p50_latency_ms\",\"p95_latency_ms\",\"p99_latency_ms\",\"max_latency_ms\"\n\
                   \"SET\",\"181818.19\",\"0.263\",\"0.088\",\"0.255\",\"0.359\",\"0.431\",\"1.031\"\n";
        let s = parse_redis_benchmark(out, Op::Set).expect("the row is there");
        assert!((s.ops - 181_818.19).abs() < 0.01);
        assert!((s.p50_us - 255.0).abs() < 0.01);
        assert!((s.p99_us - 431.0).abs() < 0.01);
    }

    #[test]
    fn mset_names_itself_with_its_key_count() {
        let out =
            "\"MSET (10 keys)\",\"90909.09\",\"0.5\",\"0.1\",\"0.4\",\"0.8\",\"0.9\",\"2.0\"\n";
        let s = parse_redis_benchmark(out, Op::Mset).expect("the row is there");
        assert!((s.ops - 90_909.09).abs() < 0.01);
    }

    #[test]
    fn the_wrong_test_is_not_reported_as_the_right_one() {
        let out = "\"GET\",\"1.0\",\"0\",\"0\",\"0\",\"0\",\"0\",\"0\"\n";
        assert!(parse_redis_benchmark(out, Op::Set).is_err());
    }

    #[test]
    fn memtier_totals_is_read_off_the_summary() {
        let out = "ALL STATS\n\
                   =========\n\
                   Type   Ops/sec  Hits/sec  Misses/sec  Avg. Latency  p50 Latency  p99 Latency  p99.9 Latency  KB/sec\n\
                   ----\n\
                   Sets   1234.00  ---       ---         1.0           0.9          2.0          3.0            10.0\n\
                   Totals 250000.50  0.00      0.00        0.98765       0.87900      2.30000      4.10000        20480.25\n";
        let s = parse_memtier(out).expect("the Totals line is there");
        assert!((s.ops - 250_000.50).abs() < 0.01);
        assert!((s.p50_us - 879.0).abs() < 0.01);
        assert!((s.p99_us - 2300.0).abs() < 0.01);
    }

    #[test]
    fn a_totals_line_that_grew_a_column_is_an_error_and_not_a_number() {
        let out = "Totals 1 2 3 4 5 6 7 8 9\n";
        let err = parse_memtier(out).expect_err("nine columns, not ten");
        assert!(err.contains("10 columns"), "{err}");
    }
}
