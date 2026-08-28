//! The checks that say a number is about the thing it claims to be about.
//!
//! `bench/00` section 5 keeps a registry of nine ways a benchmark result can be
//! wrong for a reason that has nothing to do with the code under test. Three of
//! them are checkable by this harness before it measures anything, and they are
//! the three that produced published numbers in aki's estate that later turned
//! out to be wrong:
//!
//! - C1, replica poisoning. A rival left configured as a replica of the subject
//!   is doing the subject's writes as well as its own, and comes out slower for
//!   a reason that is nothing to do with either of them.
//! - C2, pinning silently off. A `taskset` that did not take, or a layout where
//!   the server and the generator were given overlapping cores, measures the
//!   two of them fighting for a core.
//! - C3, a single threaded generator. `redis-benchmark` on one thread tops out
//!   near half a million commands a second, which is below what any of these
//!   servers can do at pipeline 16, so every row comes out as a tie at the
//!   generator's ceiling.
//!
//! Everything here refuses rather than warns. A warning in a log is a number
//! that gets published anyway.

use std::collections::BTreeSet;
use std::process::Command;

use crate::plan::{Driver, Plan};
use crate::server::Server;

/// C1: the server under test is nobody's replica and has no replicas.
///
/// Asked of every server, ours included, after it comes up and before anything
/// is measured against it. `INFO replication` is the same question the registry
/// says to ask, and the two fields it wants are `role:master` and
/// `connected_slaves:0`.
pub fn not_a_replica(srv: &Server, name: &str) -> Result<(), String> {
    let info = srv
        .ask("INFO replication")
        .map_err(|e| format!("C1: {name} would not answer INFO replication: {e}"))?;
    let role = field(&info, "role:");
    let slaves = field(&info, "connected_slaves:");
    match (role.as_deref(), slaves.as_deref()) {
        (Some("master"), Some("0")) => Ok(()),
        (Some(r), Some(s)) => Err(format!(
            "C1: {name} says role:{r} connected_slaves:{s}. A server that is replicating is doing work this run did not ask for, so nothing measured against it counts"
        )),
        _ => Err(format!(
            "C1: {name} answered INFO replication without role or connected_slaves in it, so the check cannot be made and the run does not proceed"
        )),
    }
}

/// C2: the layout that was asked for is the layout the processes got.
///
/// Two halves. The server's mask is read back from the kernel, because a
/// `taskset` that failed still leaves a process running on every core. The
/// generator's is checked by running the same `taskset` line the measured runs
/// use and asking the process it started what mask it ended up with, which
/// tests the mechanism rather than trusting it.
///
/// Linux only, because that is where the gate box is and because the mask is
/// read from `/proc`. On anything else this returns without checking, and the
/// gate box requirement in `bench/00` section 7 is what stops that from being a
/// hole: a gate row does not come from a box where the check cannot run.
pub fn pinning_took(srv: &Server, name: &str, plan: &Plan) -> Result<(), String> {
    let Some(want) = plan.server_cpus.as_deref() else {
        return Ok(());
    };
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let Some(got) = srv.affinity() else {
        return Err(format!(
            "C2: cannot read the cpu mask of {name}, so a pinned run cannot be honest about being pinned"
        ));
    };
    same_cpus(want, &got).map_err(|e| format!("C2: {name} {e}"))
}

/// C2, the generator half, plus the check that the two halves are disjoint.
///
/// Run once per session rather than per case, because it is a fact about the
/// box and the command line and not about the server that happens to be up.
pub fn pinning_layout(plan: &Plan) -> Result<(), String> {
    let (Some(srv), Some(load)) = (plan.server_cpus.as_deref(), plan.load_cpus.as_deref()) else {
        return Ok(());
    };
    let a = cpus(srv).map_err(|e| format!("C2: the server cpu list {srv:?} {e}"))?;
    let b = cpus(load).map_err(|e| format!("C2: the generator cpu list {load:?} {e}"))?;
    let shared: Vec<String> = a.intersection(&b).map(|c| c.to_string()).collect();
    if !shared.is_empty() {
        return Err(format!(
            "C2: the server and the generator were both given cpu {}, so a pinned run would be measuring the two of them taking turns on it",
            shared.join(",")
        ));
    }
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    // The generator is started as `taskset -c LIST prog`, so this asks the same
    // question the same way and reads the answer out of the process taskset
    // started.
    let out = Command::new("taskset")
        .arg("-c")
        .arg(load)
        .arg("cat")
        .arg("/proc/self/status")
        .output()
        .map_err(|e| format!("C2: taskset would not run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "C2: taskset -c {load} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let got = text
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
        .map(|v| v.trim().to_string())
        .ok_or("C2: /proc/self/status has no Cpus_allowed_list")?;
    same_cpus(load, &got).map_err(|e| format!("C2: the generator {e}"))
}

/// C3: `redis-benchmark` gets at least four threads or the run does not happen.
///
/// The registry says mandatory and means it. This is checked against the plan
/// rather than against the command line so that `--threads 2` is refused before
/// a server is started rather than after the first row is measured.
pub fn generator_threads(plan: &Plan) -> Result<(), String> {
    let drives_rb = plan
        .cases
        .iter()
        .any(|c| c.driver == Driver::RedisBenchmark);
    if drives_rb && plan.threads < 4 {
        return Err(format!(
            "C3: redis-benchmark is in this plan with {} generator thread(s). On one thread it tops out near 470,000 commands a second, which is under what these servers do at pipeline 16, so every row comes back as a tie at the generator's ceiling. Four is the floor",
            plan.threads
        ));
    }
    Ok(())
}

/// Everything checkable before a server exists.
pub fn before_the_run(plan: &Plan) -> Result<(), String> {
    generator_threads(plan)?;
    pinning_layout(plan)?;
    Ok(())
}

/// Everything checkable against a server that is up.
pub fn against(srv: &Server, name: &str, plan: &Plan) -> Result<(), String> {
    not_a_replica(srv, name)?;
    pinning_took(srv, name, plan)?;
    Ok(())
}

/// Pull `field:value` out of an INFO section.
fn field(info: &str, name: &str) -> Option<String> {
    info.lines()
        .find_map(|l| l.trim().strip_prefix(name))
        .map(|v| v.trim().to_string())
}

/// Expand a cpu list the way `taskset -c` and `/proc` both write it.
fn cpus(list: &str) -> Result<BTreeSet<u32>, String> {
    let mut out = BTreeSet::new();
    for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let a: u32 = a.trim().parse().map_err(|_| format!("has {a:?} in it"))?;
                let b: u32 = b.trim().parse().map_err(|_| format!("has {b:?} in it"))?;
                if b < a {
                    return Err(format!("has the range {part:?} backwards"));
                }
                out.extend(a..=b);
            }
            None => {
                out.insert(part.parse().map_err(|_| format!("has {part:?} in it"))?);
            }
        }
    }
    if out.is_empty() {
        return Err("is empty".to_string());
    }
    Ok(out)
}

/// Two cpu lists naming the same cores, whatever notation each of them used.
fn same_cpus(want: &str, got: &str) -> Result<(), String> {
    let a = cpus(want).map_err(|e| format!("was asked for {want:?}, which {e}"))?;
    let b = cpus(got).map_err(|e| format!("reported {got:?}, which {e}"))?;
    if a == b {
        Ok(())
    } else {
        Err(format!(
            "was asked for cpus {want} and is running on {got}, so the pinning did not take"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_and_a_list_are_the_same_cpus() {
        assert!(same_cpus("0-3", "0,1,2,3").is_ok());
        assert!(same_cpus("0-3,8", "8,0-2,3").is_ok());
    }

    #[test]
    fn a_mask_that_is_not_what_was_asked_for_is_an_error() {
        let e = same_cpus("0-3", "0-31").expect_err("the pinning did not take");
        assert!(e.contains("did not take"), "{e}");
    }

    #[test]
    fn a_backwards_range_is_an_error_and_not_an_empty_set() {
        assert!(cpus("7-3").is_err());
    }

    #[test]
    fn nonsense_in_a_cpu_list_is_an_error() {
        assert!(cpus("0-3,all").is_err());
        assert!(cpus("").is_err());
    }

    #[test]
    fn overlapping_halves_are_refused() {
        let mut plan = Plan::smoke(Vec::new(), "rb".into(), "mt".into());
        plan.server_cpus = Some("0-7".into());
        plan.load_cpus = Some("4-11".into());
        let e = pinning_layout(&plan).expect_err("cpus 4 to 7 are in both halves");
        assert!(e.starts_with("C2:"), "{e}");
        assert!(e.contains("4,5,6,7"), "{e}");
    }

    #[test]
    fn no_pinning_asked_for_is_not_a_failure() {
        let plan = Plan::smoke(Vec::new(), "rb".into(), "mt".into());
        assert!(pinning_layout(&plan).is_ok());
    }

    #[test]
    fn redis_benchmark_on_two_threads_is_refused() {
        let mut plan = Plan::smoke(Vec::new(), "rb".into(), "mt".into());
        plan.threads = 2;
        let e = generator_threads(&plan).expect_err("C3 is mandatory");
        assert!(e.starts_with("C3:"), "{e}");
    }

    #[test]
    fn four_threads_is_the_floor_and_not_a_target() {
        let mut plan = Plan::smoke(Vec::new(), "rb".into(), "mt".into());
        plan.threads = 4;
        assert!(generator_threads(&plan).is_ok());
        plan.threads = 16;
        assert!(generator_threads(&plan).is_ok());
    }

    /// A memtier only plan is not subject to C3, which is about the other one.
    #[test]
    fn a_plan_with_no_redis_benchmark_in_it_is_not_asked_about_its_threads() {
        let mut plan = Plan::smoke(Vec::new(), "rb".into(), "mt".into());
        plan.cases.retain(|c| c.driver == Driver::Memtier);
        plan.threads = 1;
        assert!(generator_threads(&plan).is_ok());
    }

    #[test]
    fn a_replication_section_is_read_field_by_field() {
        let info = "# Replication\r\nrole:master\r\nconnected_slaves:0\r\nmaster_failover_state:no-failover\r\n";
        assert_eq!(field(info, "role:").as_deref(), Some("master"));
        assert_eq!(field(info, "connected_slaves:").as_deref(), Some("0"));
        assert_eq!(field(info, "nothing:"), None);
    }
}
