//! Starting a server, waiting for it, measuring what it costs, stopping it.
//!
//! Every target goes through this, ours included. The one thing that is not
//! allowed here is a special case for `yo`: if our server needs a flag to be
//! fast then that flag is the default or the number does not count.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::plan::{Kind, Plan, Target};

/// A running server under test.
pub struct Server {
    child: Child,
    kind: Kind,
    port: u16,
    /// Highest resident set the kernel saw, in kibibytes. Zero where the
    /// platform will not say.
    peak_kb: u64,
}

impl Server {
    /// Start the target and wait until it answers.
    pub fn start(target: &Target, plan: &Plan, dir: &std::path::Path) -> io::Result<Server> {
        let mut cmd = match &plan.server_cpus {
            Some(cpus) => {
                let mut c = Command::new("taskset");
                c.arg("-c").arg(cpus).arg(&target.bin);
                c
            }
            None => Command::new(&target.bin),
        };

        match target.kind {
            Kind::Yo => {
                cmd.arg("serve")
                    .arg("--bind")
                    .arg("127.0.0.1")
                    .arg("--port")
                    .arg(plan.port.to_string());
            }
            Kind::Redis | Kind::Valkey => {
                // Persistence off on both sides. We have no file yet, so a
                // rival writing an RDB in the background would be paying for
                // something we are not doing and the row would be a lie in our
                // favour. When the file lands in M5 this comes back on for
                // everyone at once.
                cmd.arg("--port")
                    .arg(plan.port.to_string())
                    .arg("--bind")
                    .arg("127.0.0.1")
                    .arg("--save")
                    .arg("")
                    .arg("--appendonly")
                    .arg("no")
                    .arg("--protected-mode")
                    .arg("no")
                    .arg("--daemonize")
                    .arg("no")
                    .arg("--io-threads")
                    .arg(target.io_threads.to_string())
                    .arg("--dir")
                    .arg(dir);
            }
        }

        let log = std::fs::File::create(dir.join(format!("{}.log", target.name)))?;
        let errlog = log.try_clone()?;
        let child = cmd
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errlog))
            .spawn()?;

        let server = Server {
            child,
            kind: target.kind,
            port: plan.port,
            peak_kb: 0,
        };
        server.wait_ready()?;
        Ok(server)
    }

    /// Poll the port with an inline PING until it answers or we give up.
    fn wait_ready(&self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last: Option<io::Error> = None;
        while Instant::now() < deadline {
            match self.ping() {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(last.unwrap_or_else(|| io::Error::other("server never came up")))
    }

    /// One inline PING over a fresh connection.
    ///
    /// Inline rather than a RESP array because it is three lines of code and
    /// works on every server here. It is also a real end to end check: a
    /// process that has bound the port but not started reading yet will not
    /// answer it.
    fn ping(&self) -> io::Result<()> {
        let addr = format!("127.0.0.1:{}", self.port);
        let mut sock = TcpStream::connect(&addr)?;
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        sock.write_all(b"PING\r\n")?;
        let mut buf = [0u8; 7];
        let mut at = 0;
        while at < buf.len() {
            let n = sock.read(&mut buf[at..])?;
            if n == 0 {
                break;
            }
            at += n;
        }
        let _ = sock.shutdown(Shutdown::Both);
        if buf.starts_with(b"+PONG") {
            Ok(())
        } else {
            Err(io::Error::other(format!("PING answered {:?}", &buf[..at])))
        }
    }

    /// Send one inline command and throw the answer away.
    ///
    /// Used to wipe the keyspace between cases so that a GET run does not
    /// inherit whatever the SET run before it left behind.
    pub fn command(&self, line: &str) -> io::Result<()> {
        let addr = format!("127.0.0.1:{}", self.port);
        let mut sock = TcpStream::connect(&addr)?;
        sock.set_read_timeout(Some(Duration::from_secs(30)))?;
        sock.write_all(line.as_bytes())?;
        sock.write_all(b"\r\n")?;
        let mut buf = [0u8; 64];
        let _ = sock.read(&mut buf)?;
        let _ = sock.shutdown(Shutdown::Both);
        Ok(())
    }

    /// Send one inline command and read the whole bulk reply back.
    ///
    /// Only bulk strings, because the one caller asks for `INFO` and that is
    /// what it answers. A reply of any other type comes back as an error rather
    /// than as a prefix of itself, so a check built on this cannot pass by
    /// reading half of something it did not understand.
    pub fn ask(&self, line: &str) -> io::Result<String> {
        let addr = format!("127.0.0.1:{}", self.port);
        let mut sock = TcpStream::connect(&addr)?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        sock.write_all(line.as_bytes())?;
        sock.write_all(b"\r\n")?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read the header first, which is short and ends at the first newline.
        let len = loop {
            if let Some(at) = buf.iter().position(|b| *b == b'\n') {
                let head = String::from_utf8_lossy(&buf[..at]).trim().to_string();
                let Some(n) = head.strip_prefix('$').and_then(|n| n.parse::<i64>().ok()) else {
                    return Err(io::Error::other(format!("{line} answered {head:?}")));
                };
                if n < 0 {
                    return Ok(String::new());
                }
                buf.drain(..=at);
                break n as usize;
            }
            let n = sock.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::other(format!("{line} answered nothing")));
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        while buf.len() < len {
            let n = sock.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let _ = sock.shutdown(Shutdown::Both);
        buf.truncate(len);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Which cpus the kernel says this server may run on. Linux only.
    pub fn affinity(&self) -> Option<String> {
        let text = std::fs::read_to_string(format!("/proc/{}/status", self.child.id())).ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
            .map(|v| v.trim().to_string())
    }

    /// Resident set right now, in kibibytes.
    ///
    /// This is the number the memory column is built from, and it is the
    /// kernel's number rather than the server's own accounting. Redis reports
    /// `used_memory` and Valkey reports `used_memory` and neither of them
    /// includes the allocator's slack, the buffers or the code, all of which
    /// are memory the machine actually spent.
    pub fn rss_kb(&mut self) -> u64 {
        let now = rss_of(self.child.id()).unwrap_or(0);
        if now > self.peak_kb {
            self.peak_kb = now;
        }
        now
    }

    /// The highest resident set seen so far.
    pub fn peak_kb(&mut self) -> u64 {
        // `VmHWM` is the kernel's own high water mark and beats anything a
        // sampler can see, so prefer it where it exists and fall back to the
        // samples this harness took.
        let hwm = hwm_of(self.child.id()).unwrap_or(0);
        let sampled = self.rss_kb().max(self.peak_kb);
        hwm.max(sampled)
    }

    /// Ask it to stop, then make sure it did.
    pub fn stop(mut self) {
        // SHUTDOWN NOSAVE is the polite way and the one that leaves no file
        // behind. We do not implement it yet, and asking a server that does not
        // know the command to shut down means waiting out the timeout for
        // nothing on every case in the plan, so ours goes straight to the
        // signal. When SHUTDOWN lands this special case goes away.
        if self.kind != Kind::Yo {
            let _ = self.command("SHUTDOWN NOSAVE");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Resident set of a process, in kibibytes.
fn rss_of(pid: u32) -> Option<u64> {
    if let Some(v) = proc_status(pid, "VmRSS:") {
        return Some(v);
    }
    // macOS and anything else with a BSD ps. The column is already kibibytes.
    let out = Command::new("ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// High water mark of the resident set, in kibibytes. Linux only.
fn hwm_of(pid: u32) -> Option<u64> {
    proc_status(pid, "VmHWM:")
}

fn proc_status(pid: u32, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// Ask a binary what it is, for the provenance block in the report.
pub fn version_of(bin: &str) -> String {
    match Command::new(bin).arg("--version").output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text.lines().next().unwrap_or("").trim();
            if line.is_empty() {
                "unknown".to_string()
            } else {
                line.to_string()
            }
        }
        Err(_) => "unknown".to_string(),
    }
}
