//! What box this was, so a number can be argued with later.
//!
//! Every published row carries this. Most benchmark arguments on the internet
//! are two people quoting numbers from different machines at each other, and
//! the cheapest way not to be in one is to put the machine in the table.

use std::process::Command;

/// The facts about the box.
pub struct Machine {
    /// Hostname.
    pub host: String,
    /// Kernel or OS version.
    pub kernel: String,
    /// CPU model as the OS describes it.
    pub cpu: String,
    /// Logical cores.
    pub cores: u32,
    /// Total memory in mebibytes, zero if the platform will not say.
    pub memory_mib: u64,
    /// Whether the box looks like a virtual machine.
    pub virtualised: bool,
}

impl Machine {
    /// Read it off the running system.
    pub fn probe() -> Machine {
        let host = first_line(&["hostname"]).unwrap_or_else(|| "unknown".into());
        let kernel = first_line(&["uname", "-sr"]).unwrap_or_else(|| "unknown".into());

        let cpu = read_field("/proc/cpuinfo", "model name")
            .or_else(|| first_line(&["sysctl", "-n", "machdep.cpu.brand_string"]))
            .unwrap_or_else(|| "unknown".into());

        let cores = first_line(&["nproc"])
            .or_else(|| first_line(&["sysctl", "-n", "hw.ncpu"]))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let memory_mib = read_field("/proc/meminfo", "MemTotal")
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .map(|kb| kb / 1024)
            .unwrap_or(0);

        // Not a judgement, a disclaimer. A virtual machine on shared hardware
        // cannot produce a number anyone should quote as a ceiling, and the
        // report says so on the row rather than in a footnote nobody reads.
        let virtualised = std::fs::read_to_string("/sys/class/dmi/id/product_name")
            .map(|s| {
                let s = s.to_lowercase();
                s.contains("qemu")
                    || s.contains("kvm")
                    || s.contains("virtual")
                    || s.contains("vmware")
                    || s.contains("standard pc")
            })
            .unwrap_or(false);

        Machine {
            host,
            kernel,
            cpu,
            cores,
            memory_mib,
            virtualised,
        }
    }

    /// One line for the top of the report.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{}, {}, {} ({} cores)",
            self.host, self.kernel, self.cpu, self.cores
        );
        if self.memory_mib > 0 {
            s.push_str(&format!(", {} GiB", self.memory_mib / 1024));
        }
        if self.virtualised {
            s.push_str(". This is a virtual machine on shared hardware, so these numbers are a baseline and not a ceiling.");
        }
        s
    }
}

fn first_line(argv: &[&str]) -> Option<String> {
    let out = Command::new(argv[0]).args(&argv[1..]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim().to_string();
    if line.is_empty() { None } else { Some(line) }
}

fn read_field(path: &str, field: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.trim() == field
        {
            return Some(value.trim().to_string());
        }
    }
    None
}
