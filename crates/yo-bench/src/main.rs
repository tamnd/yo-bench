//! `yobench`, the thing that produces the number in the README.
//!
//! It starts a server, points a public load generator at it, reads what the
//! generator said, and does that again for the next server. Nothing in here
//! knows anything about how yo works, which is the point: the moment a
//! benchmark harness shares code with the thing it measures, it starts
//! measuring the thing it shares.
//!
//! Run it on the box under test, not across a network from it, unless the
//! number you want is a number about your network.

mod confound;
mod load;
mod machine;
mod plan;
mod report;
mod server;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use machine::Machine;
use plan::{Driver, Kind, Plan, Target};
use report::{Report, Row};
use server::Server;

const USAGE: &str = "\
yobench, the yo benchmark rig

usage:
  yobench <plan> [options]

plans:
  gate     SET, GET, INCR, MSET and the five set commands, two generators, pipeline 1 and 16
  smoke    three cases and a tenth of the load, for checking the rig works

options:
  --prefix DIR      where provision.sh put the rivals, /opt/yo-bench by default
  --yodb PATH       the yodb binary to measure
  --only NAME       only run this target, repeatable
  --requests N      commands per measured run
  --clients N       connections the generator opens
  --threads N       generator threads
  --pipeline N      only run this pipeline depth, repeatable
  --repeats N       measured runs per case, the best one is reported
  --keyspace N      how many distinct keys
  --value-bytes N   value size
  --io-threads N    io threads given to Redis and Valkey
  --pin SRV,LOAD    cpu lists for the server and the generator, for the confound run
  --socket PATH     run the load over a socket file instead of over loopback TCP
  --out DIR         where to write the report, results/ by default
  --quiet           only print the table at the end

exit codes:
  0  it ran
  1  something under test would not start or would not answer
  2  the arguments did not make sense
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("yobench: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Opts {
    plan_name: String,
    prefix: PathBuf,
    yodb: Option<PathBuf>,
    only: Vec<String>,
    requests: Option<u64>,
    clients: Option<u32>,
    threads: Option<u32>,
    pipelines: Vec<u32>,
    repeats: Option<u32>,
    keyspace: Option<u64>,
    value_bytes: Option<u32>,
    io_threads: Option<u32>,
    pin: Option<(String, String)>,
    socket: Option<String>,
    out: PathBuf,
    quiet: bool,
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some(opts) = parse(args)? else {
        return Ok(ExitCode::SUCCESS);
    };

    let targets = discover(&opts)?;
    if targets.is_empty() {
        return Err("no target to run. Was suite/provision.sh run on this box?".into());
    }

    let rb = pick(&opts.prefix, "redis-benchmark")
        .ok_or("redis-benchmark is not under the prefix. Run suite/provision.sh")?;
    // memtier is the harder one to get onto a box, because it wants autoreconf
    // and a box without it can still run every redis-benchmark row. So a
    // missing memtier drops the rows that need it and says so, rather than
    // costing the whole run.
    let mt = pick(&opts.prefix, "memtier_benchmark");

    let mut plan = match opts.plan_name.as_str() {
        "gate" => Plan::gate(targets, rb, mt.clone().unwrap_or_default()),
        "smoke" => Plan::smoke(targets, rb, mt.clone().unwrap_or_default()),
        other => return Err(format!("no such plan: {other}")),
    };

    if mt.is_none() {
        let before = plan.cases.len();
        plan.cases.retain(|c| c.driver != Driver::Memtier);
        let dropped = before - plan.cases.len();
        if plan.cases.is_empty() {
            return Err("memtier_benchmark is not under the prefix and every case needs it. Run suite/provision.sh".into());
        }
        eprintln!("memtier_benchmark is not here, dropping {dropped} of {before} cases");
    }

    if let Some(v) = opts.requests {
        plan.requests = v;
    }
    if let Some(v) = opts.clients {
        plan.clients = v;
    }
    if let Some(v) = opts.threads {
        plan.threads = v;
    }
    if let Some(v) = opts.repeats {
        plan.repeats = v;
    }
    if let Some(v) = opts.keyspace {
        plan.keyspace = v;
    }
    if let Some(v) = opts.value_bytes {
        plan.value_bytes = v;
    }
    if !opts.pipelines.is_empty() {
        plan.cases.retain(|c| opts.pipelines.contains(&c.pipeline));
    }
    if let Some(path) = &opts.socket {
        plan.socket = Some(path.clone());
        plan.name = format!("{}-socket", plan.name);
    }
    if let Some((srv, load)) = &opts.pin {
        plan.server_cpus = Some(srv.clone());
        plan.load_cpus = Some(load.clone());
    }

    let machine = Machine::probe();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = opts
        .out
        .join(format!("{}-{}-{stamp}", machine.host, plan.name));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    eprintln!("{}", machine.summary());
    eprintln!("writing to {}\n", dir.display());

    // Before anything is started, because a plan that cannot produce an honest
    // number should cost a second and not an hour.
    confound::before_the_run(&plan)?;

    preflight(&mut plan, &dir)?;
    let rows = measure(&plan, &dir, opts.quiet)?;
    let report = Report {
        plan,
        machine,
        rows,
    };

    let md = report.markdown();
    std::fs::write(dir.join("report.md"), &md).map_err(|e| format!("{e}"))?;
    std::fs::write(dir.join("run.json"), report.json()).map_err(|e| format!("{e}"))?;

    println!("{md}");
    println!("written to {}", dir.display());
    Ok(ExitCode::SUCCESS)
}

/// The most commands a calibrated case is allowed to ask for.
///
/// A guard rail on the arithmetic below and not a target. Two hundred million
/// is about twenty times what the fastest row here has ever needed, and it is
/// there so that a calibration run that came back nonsense costs a long run and
/// not a run that never ends.
const REQUEST_CEILING: u64 = 200_000_000;

/// Check every case runs, then size each one so its measured run is long enough
/// to mean something.
///
/// Two jobs and two passes, both against the first target, which is ours and is
/// the fastest thing here. A slower rival then runs for longer than the minimum,
/// which is the right way round: the number that has to be resolvable is the one
/// closest to the tick.
///
/// The first target only, for the check as well. A command line that memtier
/// refuses to parse is refused before it opens a socket, so it is not a fact
/// about the server and running it three times gives three copies of one answer.
fn preflight(plan: &mut Plan, dir: &std::path::Path) -> Result<(), String> {
    let Some(target) = plan.targets.first().cloned() else {
        return Ok(());
    };
    eprintln!(
        "checking {} case(s) against {}",
        plan.cases.len(),
        target.name
    );
    check(plan, &target, dir)?;
    if plan.min_seconds > 0.0 {
        size(plan, &target, dir)?;
    }
    eprintln!("every case runs\n");
    Ok(())
}

/// Run two thousand commands of every case, on one server, and stop at the
/// first one that will not run.
///
/// A plan is dozens of runs and it used to discover a broken command line when
/// it got to it: the gate plan died an hour in, on the first INCR case, because
/// memtier will not take `--key-pattern` and `--command-key-pattern` together,
/// by which point every SET and GET row had been measured and all of it was
/// thrown away. Two thousand commands per case turns that into a failure in the
/// first minute, and it stays a separate pass in front of the sizing below so
/// that it keeps that property: sizing costs a fill and two probes per case, so
/// a broken line found during it would be found minutes in rather than seconds.
fn check(plan: &Plan, target: &plan::Target, dir: &std::path::Path) -> Result<(), String> {
    let srv = Server::start(target, plan, dir)
        .map_err(|e| format!("{} would not start: {e}", target.name))?;
    if let Err(e) = confound::against(&srv, &target.name, plan) {
        srv.stop();
        return Err(e);
    }
    let mut out = Ok(());
    for case in &plan.cases {
        out = load::run(case.driver, case.op, plan, case.pipeline, 2000, true)
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "{} {} pipeline {} will not run: {e}",
                    case.op, case.driver, case.pipeline
                )
            });
        if out.is_err() {
            break;
        }
    }
    srv.stop();
    out
}

/// Give every case its own command count, so that its measured run lasts
/// [`Plan::min_seconds`].
///
/// One count across every case cannot be right for all of them, because a
/// pipeline 16 GET row and a pipeline 1 MSET row are twenty times apart: a
/// million commands is ten seconds of one and half a second of the other, and
/// half a second is under `redis-benchmark`'s resolution. So each case gets a
/// timed run of its own, on our clock rather than the generator's, and a count
/// that scales it up to the minimum.
fn size(plan: &mut Plan, target: &plan::Target, dir: &std::path::Path) -> Result<(), String> {
    let cases = plan.cases.clone();
    let mut sized = Vec::with_capacity(cases.len());
    for case in &cases {
        // An op whose count comes out of its fixture is not up for calibration,
        // and probing one that drains would empty the set before the measured
        // pass ever ran. Nothing to start a server for.
        if let Some(n) = case.op.fixed_requests() {
            eprintln!(
                "  {} {} pipeline {}: {n} commands, fixed by the fixture",
                case.op, case.driver, case.pipeline
            );
            sized.push(n);
            continue;
        }
        let srv = Server::start(target, plan, dir)
            .map_err(|e| format!("{} would not start: {e}", target.name))?;
        let n = size_one(plan, case);
        srv.stop();
        let n = n?;
        eprintln!(
            "  {} {} pipeline {}: {:.0} commands for {:.0}s",
            case.op, case.driver, case.pipeline, n, plan.min_seconds
        );
        sized.push(n);
    }
    for (case, n) in plan.cases.iter_mut().zip(sized) {
        case.requests = n;
    }
    Ok(())
}

/// Put the database into the state this case expects to find it in.
///
/// Two shapes and they do not overlap. GET wants a keyspace full of strings,
/// built by whichever generator is about to read it because the two of them name
/// keys differently. The set reads want particular keys holding particular
/// members, which is [`plan::Op::fixtures`] and is always built by memtier.
/// Everything else wants nothing and gets nothing.
fn setup(case: &plan::Case, plan: &Plan, quiet: bool) -> Result<(), String> {
    if case.op == plan::Op::Get {
        load::preload(case.driver, plan, quiet).map_err(|e| format!("filling: {e}"))?;
    }
    for fx in case.op.fixtures() {
        load::build(plan, fx, quiet).map_err(|e| format!("building {}: {e}", fx.key))?;
    }
    Ok(())
}

/// Time one case on a server of its own and say how many commands it needs.
///
/// A server of its own, and filled the same way, because the measured pass does
/// both and a probe against a server in some other state sizes the wrong run.
/// This was measured: sharing one server across the whole sizing pass left the
/// INCR cases probing at under a tenth of what they then measured, because by
/// the time they ran, an earlier GET case had filled the keyspace with 64 byte
/// strings and every INCR was landing on one of them and coming back an error.
/// Both INCR cases fell to the floor of a million commands and the pipeline 16
/// one measured for a third of a second.
fn size_one(plan: &Plan, case: &plan::Case) -> Result<u64, String> {
    // A GET against an empty keyspace is a miss, and a miss is faster than a
    // hit, so calibrating on one sizes the real run too short. Same argument
    // for the set reads, one step further along: SRANDMEMBER against a key that
    // is not there is a null reply and would size its run at the rate a server
    // produces nulls.
    setup(case, plan, true).map_err(|e| format!("setting up to calibrate: {e}"))?;

    // Twice, and the second one is the answer. The first probe runs into a cold
    // store: an arena that has not grown to its working size, an index that has
    // not either, and pages the process has never touched. On the first
    // calibrated gate run that was worth a third, SET at pipeline 1 probed at
    // 121 thousand a second and then measured at 186, so a case asked for ten
    // seconds of commands and finished in six and a half. The measured pass
    // warms with a tenth of its run before it counts anything, so this is the
    // same shape: pay for a warm store once, then time one.
    let mut rate = 0.0;
    for _ in 0..2 {
        let probe = load::run(
            case.driver,
            case.op,
            plan,
            case.pipeline,
            plan.requests,
            true,
        )
        .map_err(|e| format!("calibrating {} {}: {e}", case.op, case.driver))?;
        rate = plan.requests as f64 / probe.seconds.max(1e-6);
    }
    let want = (rate * plan.min_seconds).ceil() as u64;
    Ok(want.max(plan.requests).min(REQUEST_CEILING))
}

/// The measurement loop.
///
/// The server is restarted for every case rather than every plan. A GET case
/// that inherits the keyspace a SET case left behind is measuring a different
/// dataset than the one it asked for, and a server that has been running for
/// twenty cases has an allocator in a state no fresh server is in. Restarting
/// costs about a tenth of a second and removes both.
fn measure(plan: &Plan, dir: &std::path::Path, quiet: bool) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for case in &plan.cases {
        for target in &plan.targets {
            eprintln!(
                "{} {} pipeline {} on {}",
                case.op, case.driver, case.pipeline, target.name
            );

            let mut srv = Server::start(target, plan, dir)
                .map_err(|e| format!("{} would not start: {e}", target.name))?;
            // Every server, before every run, which is what the registry says.
            // Once per session would miss a rival that was made a replica
            // halfway through, and a check that only runs when nothing has
            // changed is a check that never fires.
            if let Err(e) = confound::against(&srv, &target.name, plan) {
                srv.stop();
                return Err(e);
            }

            // A read case needs something to read. Everything else builds its
            // own keys as it goes.
            let needs_setup = case.op == plan::Op::Get || !case.op.fixtures().is_empty();
            if needs_setup {
                setup(case, plan, quiet)
                    .map_err(|e| format!("setting up for {}: {e}", target.name))?;
            } else if plan.warmup {
                // A tenth of the real run, thrown away. It pays for the page
                // faults, the allocator's first growth and the branch
                // predictor, none of which are what the row is about. A case
                // that had a setup pass does not need this: filling a keyspace
                // or building a four million member set is millions of writes
                // against the same server and warms it far better than a tenth
                // of the run would have.
                let warm = (case.requests / 10).max(1000);
                load::run(case.driver, case.op, plan, case.pipeline, warm, true)
                    .map_err(|e| format!("warming {}: {e}", target.name))?;
            }

            let mut best: Option<load::Sample> = None;
            for pass in 0..plan.repeats {
                // A draining op ate its fixture last time round, so the next
                // pass gets a fresh one. Everything else leaves the setup
                // exactly as it found it and reuses it.
                if pass > 0 && case.op.drains() {
                    setup(case, plan, quiet)
                        .map_err(|e| format!("rebuilding for {}: {e}", target.name))?;
                }
                let s = load::run(
                    case.driver,
                    case.op,
                    plan,
                    case.pipeline,
                    case.requests,
                    quiet,
                )
                .map_err(|e| format!("{} on {}: {e}", case.op, target.name))?;
                // Best of, not mean. Everything that makes a run slower than
                // its neighbours is noise from something else on the box, and
                // averaging noise in does not make the number more honest, it
                // makes it a measurement of the box's other tenants.
                if best.as_ref().is_none_or(|b| s.ops > b.ops) {
                    best = Some(s);
                }
            }
            let sample = best.ok_or("repeats was zero, so nothing was measured")?;

            let rss_kb = srv.rss_kb();
            let peak_kb = srv.peak_kb();
            srv.stop();

            eprintln!(
                "  {:>12.0} ops/sec over {:.1}s, p50 {:.0} us, {} MiB peak\n",
                sample.ops,
                sample.seconds,
                sample.p50_us,
                peak_kb / 1024
            );

            rows.push(Row {
                target: target.name.clone(),
                kind: target.kind,
                version: target.version.clone(),
                op: case.op,
                driver: case.driver,
                pipeline: case.pipeline,
                ops: sample.ops,
                p50_us: sample.p50_us,
                p99_us: sample.p99_us,
                seconds: sample.seconds,
                rss_kb,
                peak_kb,
                cmdline: sample.cmdline,
            });
        }
    }
    Ok(rows)
}

/// Find what is installed and turn it into targets.
fn discover(opts: &Opts) -> Result<Vec<Target>, String> {
    let io_threads = opts.io_threads.unwrap_or(1);
    let mut out = Vec::new();

    let yodb = opts
        .yodb
        .clone()
        .or_else(|| pick(&opts.prefix, "yodb").map(PathBuf::from))
        .ok_or("no yodb binary. Pass --yodb, or put one under the prefix")?;
    out.push(target("yo", Kind::Yo, &yodb.to_string_lossy(), io_threads));

    if let Some(bin) = pick(&opts.prefix, "redis-server") {
        out.push(target("redis", Kind::Redis, &bin, io_threads));
    }
    if let Some(bin) = pick(&opts.prefix, "valkey-server") {
        out.push(target("valkey", Kind::Valkey, &bin, io_threads));
    }

    if !opts.only.is_empty() {
        out.retain(|t| opts.only.contains(&t.name));
    }
    Ok(out)
}

fn target(name: &str, kind: Kind, bin: &str, io_threads: u32) -> Target {
    Target {
        name: name.to_string(),
        kind,
        version: server::version_of(bin),
        bin: bin.to_string(),
        io_threads,
    }
}

/// Pick the newest binary with this name under the prefix, or fall back to PATH.
///
/// `provision.sh` writes `redis-server-8.10.1` rather than `redis-server`, so
/// two versions can live side by side and a run can say which one it used. The
/// sort is a plain string sort over the suffix, which orders 8.10.1 after 8.9.0
/// only by luck, so the version that gets used is printed in the report and not
/// left to be inferred from this function.
fn pick(prefix: &std::path::Path, name: &str) -> Option<String> {
    let dir = prefix.join("bin");
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f == name || f.starts_with(&format!("{name}-")))
        .collect();
    found.sort();
    if let Some(last) = found.pop() {
        return Some(dir.join(last).to_string_lossy().into_owned());
    }
    // Nothing under the prefix. Fall back to whatever is on PATH, which is
    // useful for a laptop and is why the report prints the version.
    let out = std::process::Command::new("command")
        .arg("-v")
        .arg(name)
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

fn parse(args: &[String]) -> Result<Option<Opts>, String> {
    let mut o = Opts {
        plan_name: String::new(),
        prefix: PathBuf::from("/opt/yo-bench"),
        yodb: None,
        only: Vec::new(),
        requests: None,
        clients: None,
        threads: None,
        pipelines: Vec::new(),
        repeats: None,
        keyspace: None,
        value_bytes: None,
        io_threads: None,
        pin: None,
        socket: None,
        out: PathBuf::from("results"),
        quiet: false,
    };

    let mut at = 0;
    while at < args.len() {
        let arg = args[at].as_str();
        at += 1;
        let mut value = || -> Result<String, String> {
            let v = args.get(at).ok_or(format!("{arg} needs a value"))?.clone();
            at += 1;
            Ok(v)
        };
        match arg {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "--quiet" => o.quiet = true,
            "--prefix" => o.prefix = PathBuf::from(value()?),
            "--yodb" => o.yodb = Some(PathBuf::from(value()?)),
            "--out" => o.out = PathBuf::from(value()?),
            "--only" => o.only.push(value()?),
            "--socket" => o.socket = Some(value()?),
            "--requests" => o.requests = Some(number(&value()?, arg)?),
            "--clients" => o.clients = Some(number(&value()?, arg)?),
            "--threads" => o.threads = Some(number(&value()?, arg)?),
            "--repeats" => o.repeats = Some(number(&value()?, arg)?),
            "--keyspace" => o.keyspace = Some(number(&value()?, arg)?),
            "--value-bytes" => o.value_bytes = Some(number(&value()?, arg)?),
            "--io-threads" => o.io_threads = Some(number(&value()?, arg)?),
            "--pipeline" => o.pipelines.push(number(&value()?, arg)?),
            "--pin" => {
                let v = value()?;
                let (srv, load) = v
                    .split_once(',')
                    .ok_or("--pin takes two cpu lists separated by a comma, like 0-3,4-7")?;
                o.pin = Some((srv.to_string(), load.to_string()));
            }
            other if other.starts_with('-') => return Err(format!("no such option: {other}")),
            other if o.plan_name.is_empty() => o.plan_name = other.to_string(),
            other => return Err(format!("takes one plan, and was also given {other}")),
        }
    }

    if o.plan_name.is_empty() {
        print!("{USAGE}");
        return Err("which plan?".into());
    }
    Ok(Some(o))
}

fn number<T: std::str::FromStr>(v: &str, arg: &str) -> Result<T, String> {
    v.parse().map_err(|_| format!("{arg}: {v} is not a number"))
}
