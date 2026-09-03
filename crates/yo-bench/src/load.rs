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

use crate::plan::{Driver, Fixture, Op, Plan, Shape};

/// What one measured run produced.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Commands per second, as the generator counted them.
    pub ops: f64,
    /// Median round trip in microseconds.
    pub p50_us: f64,
    /// Ninety ninth percentile round trip in microseconds.
    pub p99_us: f64,
    /// How long the generator process ran, on our clock and not on its.
    ///
    /// Taken here because the generators do not agree about elapsed time and
    /// one of them cannot be trusted about it: `redis-benchmark` stops on a 250
    /// millisecond tick and divides by what the clock said there, so its idea
    /// of how long a run took is rounded up to a multiple of a quarter second.
    /// This is a wall clock around the whole process, so it includes the
    /// generator's own startup and is an overestimate by a few milliseconds,
    /// which is the harmless direction for deciding whether a run was long
    /// enough to mean anything.
    pub seconds: f64,
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
    let (stdout, seconds, cmdline) = exec(&prog, &args, plan, quiet)?;

    let mut sample = match driver {
        Driver::RedisBenchmark => parse_redis_benchmark(&stdout, op),
        Driver::Memtier => parse_memtier(&stdout),
    }
    .map_err(|e| io::Error::other(format!("{prog}: {e}\n--- output ---\n{stdout}")))?;
    sample.cmdline = cmdline;
    sample.seconds = seconds;
    Ok(sample)
}

/// Run a generator and hand back what it said, how long it took and what it was
/// asked.
///
/// The wall clock is taken around the whole process, so it includes the
/// generator's own startup and is an overestimate by a few milliseconds. That is
/// the harmless direction for deciding whether a run was long enough to mean
/// anything.
fn exec(
    prog: &str,
    args: &[String],
    plan: &Plan,
    quiet: bool,
) -> io::Result<(String, f64, String)> {
    let mut cmd = match &plan.load_cpus {
        Some(cpus) => {
            let mut c = Command::new("taskset");
            c.arg("-c").arg(cpus).arg(prog).args(args);
            c
        }
        None => {
            let mut c = Command::new(prog);
            c.args(args);
            c
        }
    };

    let cmdline = format!("{prog} {}", args.join(" "));
    if !quiet {
        eprintln!("    {cmdline}");
    }

    let began = std::time::Instant::now();
    let out = cmd.output()?;
    let seconds = began.elapsed().as_secs_f64();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "{prog} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok((stdout, seconds, cmdline))
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

/// Put the members of one [`Fixture`] into the server.
///
/// memtier and not `redis-benchmark`, always, for the reason `Driver::can_run`
/// gives: the member names have to match what the measured run asks for and the
/// two generators name things differently. Every row that needs a fixture is a
/// memtier row, so there is one namer and no way to mix them up.
///
/// Pipeline 64 because this is setup and nobody is timing it. Five draws per
/// member for the 99.3 percent coverage the doc on `Fixture` describes, with a
/// floor so that the small algebra sets, where five times a thousand is five
/// thousand requests and takes no time at all, come out complete rather than
/// approximately complete.
pub fn build(plan: &Plan, fx: &Fixture, quiet: bool) -> io::Result<()> {
    if !quiet {
        eprintln!("    building {} with {} members", fx.key, fx.members);
    }
    let (prog, args) = fixture_args(plan, fx);
    exec(&prog, &args, plan, quiet).map(|_| ())
}

fn fixture_args(plan: &Plan, fx: &Fixture) -> (String, Vec<String>) {
    // Five draws a member for a set, because a draw can miss what a walk cannot
    // and the doc on `Fixture` works out what that costs. One append an entry
    // for a stream, because an append never misses: it does not care what was
    // appended before it and two of them writing the same `__key__` are still
    // two entries.
    let requests = match fx.shape {
        Shape::Set => fx.members.saturating_mul(5).max(100_000),
        Shape::Stream => fx.members,
    };
    let per_thread = (plan.clients / plan.threads).max(1);
    let conns = per_thread * plan.threads;
    let per_conn = (requests / u64::from(conns)).max(1);

    let mut args = memtier_where(plan);
    args.extend([
        "-P".into(),
        "redis".into(),
        "-t".into(),
        plan.threads.to_string(),
        "-c".into(),
        per_thread.to_string(),
        "-n".into(),
        per_conn.to_string(),
        "--pipeline=64".into(),
        format!("--key-minimum={}", fx.from),
        format!("--key-maximum={}", fx.to()),
        "--hide-histogram".into(),
        "--distinct-client-seed".into(),
        match fx.shape {
            Shape::Set => format!("--command=SADD {} __key__", fx.key),
            // A star for the id, so the ids are the ones a real producer gets
            // and the fill can come off every connection at once. Nothing reads
            // the field, and it is `__key__` rather than a literal so that both
            // shapes are the same invocation with the same key range.
            Shape::Stream => format!("--command=XADD {} * f __key__", fx.key),
        },
        "--command-key-pattern=R".into(),
    ]);
    (plan.memtier.clone(), args)
}

/// Where memtier should connect, and never both ways at once.
///
/// memtier spells the socket file `-S` and the host `-s`. Passing a host and a
/// socket together is not an error and the socket wins, which is exactly the
/// kind of thing that produces a report labelled one way and measured the
/// other, so only one of them is ever built.
fn memtier_where(plan: &Plan) -> Vec<String> {
    match &plan.socket {
        Some(path) => vec!["-S".into(), path.clone()],
        None => vec![
            "-s".into(),
            "127.0.0.1".into(),
            "-p".into(),
            plan.port.to_string(),
        ],
    }
}

fn redis_benchmark_args(
    op: Op,
    plan: &Plan,
    pipeline: u32,
    requests: u64,
) -> (String, Vec<String>) {
    // `-s` takes a socket file and `-h`/`-p` take a port. Passing both is not
    // an error and the socket wins, which is exactly the kind of thing that
    // produces a report labelled one way and measured the other, so only one
    // of them is ever passed.
    let mut args = match &plan.socket {
        Some(path) => vec!["-s".into(), path.clone()],
        None => vec![
            "-h".into(),
            "127.0.0.1".into(),
            "-p".into(),
            plan.port.to_string(),
        ],
    };
    args.extend([
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
    ]);
    (plan.redis_benchmark.clone(), args)
}

fn memtier_args(op: Op, plan: &Plan, pipeline: u32, requests: u64) -> (String, Vec<String>) {
    // `-c` is connections per thread here, not connections. The plan counts
    // connections, so this is where that gets divided, and `-n` is per
    // connection so the total gets divided by both.
    let per_thread = (plan.clients / plan.threads).max(1);
    let conns = per_thread * plan.threads;
    let per_conn = (requests / u64::from(conns)).max(1);

    let mut args = memtier_where(plan);
    args.extend([
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
    ]);
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
        // One literal key for the whole run and the random one as the member,
        // which is the same shape `redis-benchmark -t sadd` sends: it writes
        // `SADD myset:{tag} element:__rand_int__`, one key with a member out of
        // the keyspace. The key pattern still applies, because it governs
        // `__key__` and not the position it appears in.
        Op::Sadd => {
            args.push("--command=SADD myset __key__".into());
            args.push("--command-key-pattern=R".into());
        }
        // The fixture rows name their keys literally and take no argument out
        // of the keyspace, so like PING they get no key pattern. The keys have
        // to be the ones `Op::fixtures` builds or the row measures a miss.
        // The same command `redis-benchmark -t xadd` sends, down to the star
        // and the uncapped stream, so the two generators are measuring one
        // thing. The field value comes out of the keyspace the way the set
        // member does.
        Op::Xadd => {
            args.push("--command=XADD stream:add * f __key__".into());
            args.push("--command-key-pattern=R".into());
        }
        Op::Xlen => args.push("--command=XLEN stream:read".into()),
        Op::Xrange => args.push("--command=XRANGE stream:read - + COUNT 10".into()),
        Op::Spop => args.push("--command=SPOP set:pop".into()),
        Op::Srandmember => args.push("--command=SRANDMEMBER set:rand".into()),
        Op::Sinter => args.push("--command=SINTER set:a set:b".into()),
        Op::Sunion => args.push("--command=SUNION set:a set:b".into()),
        // No key in it, so no key pattern either. Passing one is the error
        // described above rather than a setting that does nothing.
        Op::Ping => args.push("--command=PING".into()),
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
            seconds: 0.0,
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
            seconds: 0.0,
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

    /// One transport per command line. Both generators accept a host and a
    /// socket file at the same time and quietly pick one, so the test that
    /// matters is not that the right flag is there but that the other one is
    /// not.
    #[test]
    fn a_tcp_run_names_a_host_and_a_port_and_no_socket_file() {
        let plan = Plan::smoke(Vec::new(), "redis-benchmark".into(), "memtier".into());
        let (_, rb) = redis_benchmark_args(Op::Get, &plan, 1, 1000);
        assert!(window(&rb, "-h") == Some("127.0.0.1"));
        assert!(window(&rb, "-p") == Some(&plan.port.to_string()[..]));
        assert!(!rb.iter().any(|a| a == "-s"), "{rb:?}");

        let (_, mt) = memtier_args(Op::Get, &plan, 1, 1000);
        assert!(window(&mt, "-s") == Some("127.0.0.1"));
        assert!(window(&mt, "-p") == Some(&plan.port.to_string()[..]));
        assert!(!mt.iter().any(|a| a == "-S"), "{mt:?}");
    }

    #[test]
    fn a_socket_run_names_the_file_and_no_host_or_port() {
        let mut plan = Plan::smoke(Vec::new(), "redis-benchmark".into(), "memtier".into());
        plan.socket = Some("/tmp/yobench.sock".into());

        let (_, rb) = redis_benchmark_args(Op::Get, &plan, 1, 1000);
        assert_eq!(window(&rb, "-s"), Some("/tmp/yobench.sock"));
        assert!(!rb.iter().any(|a| a == "-h" || a == "-p"), "{rb:?}");

        // memtier's `-S` is the socket and its lowercase `-s` is the host, so
        // this is the one place where the two generators disagree about the
        // spelling and the wrong letter would still run and still produce a
        // number, over the wrong transport.
        let (_, mt) = memtier_args(Op::Get, &plan, 1, 1000);
        assert_eq!(window(&mt, "-S"), Some("/tmp/yobench.sock"));
        assert!(!mt.iter().any(|a| a == "-s" || a == "-p"), "{mt:?}");
    }

    /// The two generators have to send the same shape or the row is two
    /// different benchmarks sharing a name. Both send one literal key for the
    /// whole run with the random draw as the member, which is what makes this
    /// the hot key row rather than another spread write.
    #[test]
    fn both_generators_send_sadd_at_one_key_with_a_random_member() {
        let plan = Plan::smoke(Vec::new(), "redis-benchmark".into(), "memtier".into());

        let (_, rb) = redis_benchmark_args(Op::Sadd, &plan, 1, 1000);
        assert_eq!(window(&rb, "-t"), Some("sadd"));

        let (_, mt) = memtier_args(Op::Sadd, &plan, 1, 1000);
        assert!(
            mt.iter().any(|a| a == "--command=SADD myset __key__"),
            "{mt:?}"
        );
        assert!(mt.iter().any(|a| a == "--command-key-pattern=R"), "{mt:?}");
        // The other spelling is an error and not a duplicate, and it kills the
        // run an hour in rather than at the start.
        assert!(!mt.iter().any(|a| a.starts_with("--key-pattern")), "{mt:?}");
    }

    /// The test that earns its keep. A fixture builds `set:pop` and the
    /// measured run sends `SPOP set:pop`, and those two strings live in
    /// different files. Let them drift by one character and the run pops from a
    /// key that is not there, which does not fail, does not warn, and comes back
    /// faster than the real thing because a null reply is cheaper than a member.
    /// The row would be published as a set benchmark and would be a benchmark of
    /// missing keys.
    #[test]
    fn every_fixture_row_asks_for_the_keys_its_fixtures_build() {
        let plan = Plan::smoke(Vec::new(), "redis-benchmark".into(), "memtier".into());
        for op in [
            Op::Spop,
            Op::Srandmember,
            Op::Sinter,
            Op::Sunion,
            Op::Xlen,
            Op::Xrange,
        ] {
            let (_, args) = memtier_args(op, &plan, 1, 1000);
            let command = args
                .iter()
                .find(|a| a.starts_with("--command="))
                .unwrap_or_else(|| panic!("{op} sends a command"));
            let sent: Vec<&str> = command
                .trim_start_matches("--command=")
                .split_whitespace()
                .skip(1)
                .collect();
            let built: Vec<&str> = op.fixtures().iter().map(|f| f.key).collect();
            // The keys come first and whatever follows them is the command's
            // own options, which XRANGE has and the set rows do not.
            assert!(
                sent.len() >= built.len() && sent[..built.len()] == built[..],
                "{op} reads {sent:?} and builds {built:?}"
            );
        }
    }

    /// The fixture builder is memtier writing SADD over a range, and the range
    /// is the whole point: it is what decides how much two sets share.
    #[test]
    fn a_fixture_is_built_over_the_range_it_names() {
        let plan = Plan::smoke(Vec::new(), "redis-benchmark".into(), "memtier".into());
        let fx = Fixture {
            shape: Shape::Set,
            key: "set:a",
            from: 501,
            members: 1_000,
        };
        let args = fixture_args(&plan, &fx).1;
        assert!(args.iter().any(|a| a == "--key-minimum=501"), "{args:?}");
        assert!(args.iter().any(|a| a == "--key-maximum=1500"), "{args:?}");
        assert!(
            args.iter().any(|a| a == "--command=SADD set:a __key__"),
            "{args:?}"
        );
        assert!(
            args.iter().any(|a| a == "--command-key-pattern=R"),
            "{args:?}"
        );
        // Same trap as the measured rows: memtier refuses to be given both
        // spellings and dies with a usage message rather than a warning.
        assert!(
            !args.iter().any(|a| a.starts_with("--key-pattern")),
            "{args:?}"
        );
    }

    /// A stream fixture is filled with appends and one of them an entry.
    ///
    /// Five times the range is right for a set, where a draw can pick the same
    /// member twice and the fifth draw is what gets the coverage to 99.3
    /// percent. It is wrong for a stream, where it would build five million
    /// entries and call them one million.
    #[test]
    fn a_stream_fixture_is_appended_to_once_an_entry() {
        let plan = Plan::smoke(Vec::new(), "redis-benchmark".into(), "memtier".into());
        let fx = Fixture {
            shape: Shape::Stream,
            key: "stream:read",
            from: 1,
            members: 1_000_000,
        };
        let args = fixture_args(&plan, &fx).1;
        assert!(
            args.iter()
                .any(|a| a == "--command=XADD stream:read * f __key__"),
            "{args:?}"
        );
        let conns = u64::from(plan.clients.max(1));
        let per_conn: u64 = args
            .iter()
            .position(|a| a == "-n")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .expect("a request count");
        // Within one command a connection of the nominal size, which is the
        // integer division every row here does and not something this shape
        // introduced.
        let total = per_conn * conns;
        assert!(
            total <= 1_000_000 && 1_000_000 - total < conns,
            "{total} entries against a million, {args:?}"
        );
    }

    /// A fixture goes over whichever transport the run is using, or it fills a
    /// server the measured pass is not talking to.
    #[test]
    fn a_fixture_follows_the_run_onto_the_socket_file() {
        let mut plan = Plan::smoke(Vec::new(), "redis-benchmark".into(), "memtier".into());
        plan.socket = Some("/tmp/yobench.sock".into());
        let args = fixture_args(&plan, &Op::Spop.fixtures()[0]).1;
        assert_eq!(window(&args, "-S"), Some("/tmp/yobench.sock"));
        assert!(!args.iter().any(|a| a == "-s" || a == "-p"), "{args:?}");
    }

    /// The value that follows a flag, or None if the flag is not there.
    fn window<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == flag)?;
        args.get(at + 1).map(|s| s.as_str())
    }

    #[test]
    fn a_totals_line_that_grew_a_column_is_an_error_and_not_a_number() {
        let out = "Totals 1 2 3 4 5 6 7 8 9\n";
        let err = parse_memtier(out).expect_err("nine columns, not ten");
        assert!(err.contains("10 columns"), "{err}");
    }
}
