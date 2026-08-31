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
#[derive(Debug)]
pub struct Server {
    child: Child,
    kind: Kind,
    port: u16,
    /// Where this server's output went, so a failure can quote it rather than
    /// tell you to go looking.
    log: std::path::PathBuf,
    /// Highest resident set the kernel saw, in kibibytes. Zero where the
    /// platform will not say.
    peak_kb: u64,
}

impl Server {
    /// Start the target and wait until it answers.
    ///
    /// The port is checked before the spawn and after the stop, and the child is
    /// watched while we wait for it. All three are the same bug seen from
    /// different ends, and it is worth saying what it was because it cost a
    /// whole gate run and produced numbers that looked fine.
    ///
    /// A server left over from an earlier run was still holding the port. Every
    /// `start` after that spawned a child that died on "Address already in use",
    /// and `wait_ready` then pinged the port, got `+PONG` from the leftover, and
    /// said the server was up. So the harness restarted the server between every
    /// case, as its own doc comment promises, and none of those restarts did
    /// anything: one process served the entire plan and every case inherited the
    /// keyspace the case before it had built. The run died nine cases in, on
    /// `INCR` against keys that the `SET` case had filled with random strings,
    /// and the eight rows before it were measured against a server in a state
    /// nobody had asked for.
    pub fn start(target: &Target, plan: &Plan, dir: &std::path::Path) -> io::Result<Server> {
        // Nothing may be on the port before we start. One server runs at a time
        // here, so an answer now is a leftover, and carrying on would measure it
        // instead of the binary this call names.
        if ping_port(plan.port).is_ok() {
            return Err(io::Error::other(format!(
                "something is already answering on port {}, so {} was not started. \
                 It is a server left over from an earlier run. Stop it and try again.",
                plan.port, target.name
            )));
        }

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
                if let Some(path) = &plan.socket {
                    cmd.arg("--unixsocket").arg(path);
                }
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
                if let Some(path) = &plan.socket {
                    // The port stays open on every server, socket file run or
                    // not, because the readiness check, the C1 check and the
                    // shutdown all go over it. Only the load moves.
                    cmd.arg("--unixsocket").arg(path);
                }
            }
        }

        if let Some(path) = &plan.socket {
            // A server that was killed rather than asked to stop leaves the
            // path behind, and Redis refuses to bind one that exists. One
            // server runs at a time here, so anything at that path is a
            // leftover from the last case and not somebody else's socket.
            let _ = std::fs::remove_file(path);
        }

        let path = dir.join(format!("{}.log", target.name));
        let log = std::fs::File::create(&path)?;
        let errlog = log.try_clone()?;
        let child = cmd
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errlog))
            .spawn()?;

        let mut server = Server {
            child,
            kind: target.kind,
            port: plan.port,
            log: path,
            peak_kb: 0,
        };
        server.wait_ready()?;
        Ok(server)
    }

    /// Poll the port with an inline PING until our own child answers it.
    ///
    /// The child is checked first on every turn of the loop, because a server
    /// that could not bind is gone within milliseconds and a ping that succeeds
    /// after that came from somebody else. Answering the port is not the same
    /// question as being the process we started, and this is where those two
    /// were run together.
    fn wait_ready(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last: Option<io::Error> = None;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(io::Error::other(format!(
                    "the server exited with {status} before it answered. Its output was: {}",
                    self.log_tail()
                )));
            }
            match self.ping() {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(io::Error::other(format!(
            "the server never came up: {}. Its output was: {}",
            last.map_or_else(|| "no reason given".to_string(), |e| e.to_string()),
            self.log_tail()
        )))
    }

    /// The last few lines the server wrote, on one line, for an error message.
    fn log_tail(&self) -> String {
        let Ok(text) = std::fs::read_to_string(&self.log) else {
            return format!("nothing readable at {}", self.log.display());
        };
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return "nothing at all".to_string();
        }
        let from = lines.len().saturating_sub(5);
        lines[from..].join(" / ")
    }

    /// One inline PING over a fresh connection.
    ///
    /// Inline rather than a RESP array because it is three lines of code and
    /// works on every server here. It is also a real end to end check: a
    /// process that has bound the port but not started reading yet will not
    /// answer it.
    fn ping(&self) -> io::Result<()> {
        ping_port(self.port)
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

    /// Ask it to stop, then make sure it did, then make sure the port is free.
    ///
    /// The second half is the other end of the check in [`Server::start`]. If
    /// something is still answering after our child is gone, then either it was
    /// never our child answering or we have leaked one, and both of those are
    /// worth a line on the terminal at the moment they happen rather than a
    /// confusing failure in the next case.
    pub fn stop(mut self) {
        let port = self.port;
        self.stop_child();
        // A moment for the socket to come out of the accept queue, then look.
        std::thread::sleep(Duration::from_millis(50));
        if ping_port(port).is_ok() {
            eprintln!(
                "warning: port {port} is still answering after the server was stopped. \
                 There is another server on it and the rows after this one measure that one."
            );
        }
    }

    fn stop_child(&mut self) {
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

/// One inline PING to a port, whoever is on the other end of it.
///
/// A free function rather than a method because [`Server::start`] asks the
/// question before it has a server to ask it about, and that is the whole point:
/// the answer says only that something is there, not that it is ours.
fn ping_port(port: u16) -> io::Result<()> {
    let addr = format!("127.0.0.1:{port}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Driver, Op};

    /// A listener that answers `+PONG` and nothing else, standing in for a
    /// server left over from an earlier run.
    fn squatter() -> (u16, std::sync::mpsc::Sender<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().expect("bound").port();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("the listener takes nonblocking");
            while rx.try_recv().is_err() {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let mut buf = [0u8; 64];
                        let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
                        let _ = sock.read(&mut buf);
                        let _ = sock.write_all(b"+PONG\r\n");
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        (port, tx)
    }

    fn plan_on(port: u16) -> Plan {
        let mut plan = Plan::gate(Vec::new(), "redis-benchmark".into(), "memtier".into());
        plan.port = port;
        plan.cases = vec![crate::plan::Case {
            op: Op::Ping,
            driver: Driver::Memtier,
            pipeline: 1,
            requests: 1,
        }];
        plan
    }

    fn target(bin: &str) -> Target {
        Target {
            name: "yo".into(),
            kind: Kind::Yo,
            bin: bin.into(),
            version: "test".into(),
            io_threads: 1,
        }
    }

    /// The bug that cost a gate run, from the front. Starting onto a port that
    /// already answers has to be an error, because the alternative is measuring
    /// whatever is on it.
    #[test]
    fn a_server_already_on_the_port_is_refused() {
        let (port, stop) = squatter();
        let dir = std::env::temp_dir();
        // The binary does not exist and does not need to. Refusing happens
        // before the spawn, and a name that could not run is the clearest way
        // to say the spawn never happened.
        let err = Server::start(&target("no-such-server"), &plan_on(port), &dir)
            .expect_err("starting onto a held port has to fail");
        let msg = err.to_string();
        assert!(msg.contains("already answering"), "{msg}");
        let _ = stop.send(());
    }

    /// And from the back. A binary that exits without binding must be reported
    /// as having exited, rather than waited out for twenty seconds or, worse,
    /// declared up because something else answered.
    #[test]
    fn a_server_that_exits_immediately_is_reported() {
        // A free port with nothing on it: bound to find the number, then
        // dropped. Racy in principle, but nothing else in this test binary
        // binds and the window is microseconds.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("a free port")
            .local_addr()
            .expect("bound")
            .port();
        let dir = std::env::temp_dir();
        let started = Instant::now();
        let err = Server::start(&target("/usr/bin/false"), &plan_on(port), &dir)
            .expect_err("a server that exits cannot be ready");
        let msg = err.to_string();
        assert!(msg.contains("exited with"), "{msg}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it waited out the readiness timeout instead of noticing the child was gone"
        );
    }
}
