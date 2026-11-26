//! `crabd probe <name> [args]`: the in-sandbox half of the self-test (design §8).
//!
//! The host starts each probe through `process.open` as an ordinary program, from the `crabd`
//! binary every sandbox already has, so an image without a shell can be tested. A probe
//! tries one thing and prints **one JSON line** on stdout: what it tried and what it got (an
//! errno name such as `"ENETUNREACH"`, `"ok"`, or `"timeout"`). It never judges: the host
//! decides pass, fail or inconclusive (`sandcrab_sdk::selftest`). The exit status is 0 when a
//! line was printed, 2 for a usage error.
//!
//! | Probe | Arguments | Prints |
//! | --- | --- | --- |
//! | `caps-empty`, `no-new-privs` | | `status`: the `Cap*` and `NoNewPrivs` fields of `/proc/self/status` |
//! | `userns-refused` | | `result` of `unshare(CLONE_NEWUSER)` |
//! | `net-loopback-only` | | `interfaces` from `/proc/net/dev` |
//! | `net-external-refused`, `gateway-refused`, `metadata-refused` | | `attempts`: TCP connects |
//! | `dns-refused` | | `attempts`: one UDP query per resolver in `/etc/resolv.conf` and a public one |
//! | `forward-reaches-target`, `forward-only-declared`, `forward-fails-closed` | `<port> [<nonce>]` | `attempt` on `127.0.0.1:<port>`; with a nonce: sent, `reply`, `end` |
//! | `fs-outside-mounts` | `--absent <path>`... `--read <file>` `--ro <dir>` `--rw <file> <nonce>` | each path's result |
//! | `other-sandbox-unreachable` | `<port> <nonce>` | `attempt` on `127.0.0.1:<port>`, `visible`: pids whose command line has the nonce |
//! | `exec-env-exact` | | `env`: its whole environment |
//! | `tree-kill` | `<nonce>` | starts a child in a new session that survives `SIGTERM`, prints its pid once it runs, then waits for ever |
//! | `listen` | `<port> <nonce>` | listens on `127.0.0.1:<port>`, prints `listening`, waits for ever (the other sandbox's half) |
//! | `find` | `<nonce>` | `visible`: pids whose command line has the nonce (this probe excluded) |
//!
//! Every network attempt has a deadline of [`ATTEMPT_TIMEOUT`]; attempts of one probe run at
//! once, so a probe answers well within the host's 5 s.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Deadline of one connect, send or read.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Public addresses that must be unreachable (literal: no DNS involved).
const EXTERNAL: &[&str] = &["1.1.1.1:443", "8.8.8.8:443"];
/// A public resolver, tried on top of the ones in `/etc/resolv.conf`.
const PUBLIC_RESOLVER: &str = "1.1.1.1:53";
/// Docker's default bridge, QEMU/slirp user networking, gvproxy.
const GATEWAYS: &[&str] = &["172.17.0.1:80", "10.0.2.2:80", "192.168.127.1:80"];
/// The cloud metadata service.
const METADATA: &str = "169.254.169.254:80";

/// Entry point of `crabd probe`, called with the arguments after `probe`.
pub fn main(args: &[OsString]) -> ExitCode {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some((name, rest)) = args.split_first() else {
        return usage("missing probe name");
    };
    let out = match (name.as_str(), rest) {
        ("caps-empty" | "no-new-privs", []) => status(),
        ("userns-refused", []) => userns(),
        ("net-loopback-only", []) => interfaces(),
        ("net-external-refused", []) => tcp_attempts(EXTERNAL),
        ("gateway-refused", []) => tcp_attempts(GATEWAYS),
        ("metadata-refused", []) => tcp_attempts(&[METADATA]),
        ("dns-refused", []) => dns(),
        (
            "forward-reaches-target" | "forward-only-declared" | "forward-fails-closed",
            [port, nonce @ ..],
        ) if nonce.len() <= 1 => match port.parse() {
            Ok(port) => loopback_exchange(port, nonce.first().map(String::as_str)),
            Err(_) => return usage("bad port"),
        },
        ("fs-outside-mounts", rest) => match fs_outside_mounts(rest) {
            Some(v) => v,
            None => return usage("bad fs-outside-mounts arguments"),
        },
        ("other-sandbox-unreachable", [port, nonce]) => match port.parse() {
            Ok(port) => json!({
                "attempt": tcp_attempt(SocketAddr::from((Ipv4Addr::LOCALHOST, port))),
                "visible": with_nonce(nonce),
            }),
            Err(_) => return usage("bad port"),
        },
        ("exec-env-exact", []) => {
            let env: BTreeMap<String, String> = std::env::vars_os()
                .map(|(k, v)| (lossy(&k), lossy(&v)))
                .collect();
            json!({ "env": env })
        }
        ("tree-kill", [nonce]) => return tree_kill(nonce),
        ("tree-kill-child", [_nonce]) => return tree_kill_child(),
        ("listen", [port, _nonce]) => match port.parse() {
            Ok(port) => return listen(port),
            Err(_) => return usage("bad port"),
        },
        ("find", [nonce]) => json!({ "visible": with_nonce(nonce) }),
        _ => return usage(&format!("unknown probe or arguments: {args:?}")),
    };
    print_line(&out)
}

fn usage(why: &str) -> ExitCode {
    eprintln!("crabd probe: {why}");
    ExitCode::from(2)
}

fn print_line(v: &Value) -> ExitCode {
    let mut out = io::stdout().lock();
    match writeln!(out, "{v}").and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn lossy(s: &std::ffi::OsStr) -> String {
    s.to_string_lossy().into_owned()
}

/// `"ok"`, `"timeout"`, an errno name (`"ENETUNREACH"`), or `"errno <n>"`.
#[must_use]
pub fn result_name(r: &io::Result<()>) -> String {
    match r {
        Ok(()) => "ok".into(),
        Err(e) => error_name(e),
    }
}

/// See [`result_name`].
#[must_use]
pub fn error_name(e: &io::Error) -> String {
    use rustix::io::Errno;
    let Some(raw) = e.raw_os_error() else {
        return match e.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => "timeout".into(),
            kind => format!("{kind:?}"),
        };
    };
    let errno = Errno::from_raw_os_error(raw);
    let known = [
        (Errno::PERM, "EPERM"),
        (Errno::NOENT, "ENOENT"),
        (Errno::ACCESS, "EACCES"),
        (Errno::ROFS, "EROFS"),
        (Errno::INVAL, "EINVAL"),
        (Errno::NOSPC, "ENOSPC"),
        (Errno::NOSYS, "ENOSYS"),
        (Errno::NETUNREACH, "ENETUNREACH"),
        (Errno::HOSTUNREACH, "EHOSTUNREACH"),
        (Errno::CONNREFUSED, "ECONNREFUSED"),
        (Errno::CONNRESET, "ECONNRESET"),
        (Errno::PIPE, "EPIPE"),
        (Errno::ADDRNOTAVAIL, "EADDRNOTAVAIL"),
        (Errno::AFNOSUPPORT, "EAFNOSUPPORT"),
        (Errno::NXIO, "ENXIO"),
    ];
    match known.iter().find(|(n, _)| *n == errno) {
        Some((_, name)) => (*name).into(),
        // EAGAIN on a socket read timeout, ETIMEDOUT from the kernel.
        None if errno == Errno::AGAIN || errno == Errno::TIMEDOUT => "timeout".into(),
        None => format!("errno {raw}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Process attributes
// ---------------------------------------------------------------------------------------------

fn status() -> Value {
    match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => {
            let fields: BTreeMap<&str, &str> = s
                .lines()
                .filter_map(|l| l.split_once(':'))
                .filter(|(k, _)| k.starts_with("Cap") || *k == "NoNewPrivs")
                .map(|(k, v)| (k, v.trim()))
                .collect();
            json!({ "tried": "read /proc/self/status", "status": fields })
        }
        Err(e) => json!({ "tried": "read /proc/self/status", "result": error_name(&e) }),
    }
}

/// `unshare(CLONE_NEWUSER)`. `crabd probe` runs no thread before this (it is dispatched
/// before any runtime starts), so a multi-threaded `EINVAL` cannot pass as a refusal.
fn userns() -> Value {
    #[cfg(target_os = "linux")]
    let r: io::Result<()> = {
        #[allow(deprecated)] // safe for CLONE_NEWUSER; `unshare_unsafe` is about CLONE_FILES
        let r = rustix::thread::unshare(rustix::thread::UnshareFlags::NEWUSER);
        r.map_err(io::Error::from)
    };
    #[cfg(not(target_os = "linux"))]
    let r: io::Result<()> = Err(io::Error::from(io::ErrorKind::Unsupported));
    json!({ "tried": "unshare(CLONE_NEWUSER)", "result": result_name(&r) })
}

// ---------------------------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------------------------

fn interfaces() -> Value {
    match std::fs::read_to_string("/proc/net/dev") {
        Ok(s) => {
            let names: Vec<&str> = s
                .lines()
                .skip(2)
                .filter_map(|l| l.split_once(':').map(|(n, _)| n.trim()))
                .collect();
            json!({ "tried": "read /proc/net/dev", "interfaces": names })
        }
        Err(e) => json!({ "tried": "read /proc/net/dev", "result": error_name(&e) }),
    }
}

fn tcp_attempt(addr: SocketAddr) -> Value {
    let start = Instant::now();
    let r = TcpStream::connect_timeout(&addr, ATTEMPT_TIMEOUT).map(drop);
    json!({
        "target": format!("tcp {addr}"),
        "result": result_name(&r),
        "ms": start.elapsed().as_millis(),
    })
}

/// Run `f` on each item at once (scoped threads) and keep the order.
fn concurrently<T: Sync>(items: &[T], f: impl Fn(&T) -> Value + Sync) -> Vec<Value> {
    std::thread::scope(|s| {
        let handles: Vec<_> = items.iter().map(|i| s.spawn(|| f(i))).collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(Value::Null))
            .collect()
    })
}

fn tcp_attempts(targets: &[&str]) -> Value {
    let addrs: Vec<SocketAddr> = targets.iter().filter_map(|t| t.parse().ok()).collect();
    json!({ "attempts": concurrently(&addrs, |a| tcp_attempt(*a)) })
}

/// The `nameserver` lines of a `resolv.conf`, as port-53 addresses.
#[must_use]
pub fn resolvers(conf: &str) -> Vec<SocketAddr> {
    conf.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|a| a.trim().split('%').next()?.parse::<IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 53))
        .collect()
}

/// One UDP datagram to `addr`, then wait for an answer: the first error wins. A loopback
/// resolver with nothing behind it answers with `ECONNREFUSED` on the read.
fn udp_attempt(addr: SocketAddr) -> Value {
    let start = Instant::now();
    let bind: SocketAddr = match addr {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let r = (|| {
        let sock = UdpSocket::bind(bind)?;
        sock.connect(addr)?;
        // A DNS query header for `.` IN A: any resolver answers something.
        sock.send(&[0x53, 0x43, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1])?;
        sock.set_read_timeout(Some(ATTEMPT_TIMEOUT))?;
        let mut buf = [0u8; 512];
        sock.recv(&mut buf).map(drop)
    })();
    json!({
        "target": format!("udp {addr}"),
        "result": result_name(&r),
        "ms": start.elapsed().as_millis(),
    })
}

fn dns() -> Value {
    let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let mut addrs = resolvers(&conf);
    if let Ok(public) = PUBLIC_RESOLVER.parse() {
        addrs.push(public);
    }
    json!({ "attempts": concurrently(&addrs, |a| udp_attempt(*a)) })
}

/// Connect to `127.0.0.1:<port>`. With a nonce: send it, half-close, read the reply to its
/// end. `end` is how the read ended (`"eof"`, an errno, `"timeout"`).
fn loopback_exchange(port: u16, nonce: Option<&str>) -> Value {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let start = Instant::now();
    let stream = match TcpStream::connect_timeout(&addr, ATTEMPT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            return json!({ "attempt": {
                "target": format!("tcp {addr}"),
                "result": error_name(&e),
                "ms": start.elapsed().as_millis(),
            }});
        }
    };
    let attempt = json!({
        "target": format!("tcp {addr}"),
        "result": "ok",
        "ms": start.elapsed().as_millis(),
    });
    let Some(nonce) = nonce else {
        return json!({ "attempt": attempt });
    };
    let mut stream = stream;
    // A peer that already closed may refuse the write; the read below says how it ended.
    let sent = stream
        .write_all(nonce.as_bytes())
        .and_then(|()| stream.shutdown(Shutdown::Write));
    let _ = stream.set_read_timeout(Some(ATTEMPT_TIMEOUT));
    let mut reply = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        match stream.read(&mut buf) {
            Ok(0) => break "eof".to_owned(),
            Ok(n) if reply.len() < 64 * 1024 => reply.extend_from_slice(&buf[..n]),
            Ok(_) => break "too much".to_owned(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => break error_name(&e),
        }
    };
    json!({
        "attempt": attempt,
        "sent": result_name(&sent),
        "reply": String::from_utf8_lossy(&reply),
        "bytes": reply.len(),
        "end": end,
    })
}

/// Pids (not this one) whose `/proc/<pid>/cmdline` contains `nonce`.
fn with_nonce(nonce: &str) -> Vec<u32> {
    let me = std::process::id();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != me)
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| c.windows(nonce.len().max(1)).any(|w| w == nonce.as_bytes()))
        })
        .collect();
    pids.sort_unstable();
    pids
}

fn listen(port: u16) -> ExitCode {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(l) => l,
        Err(e) => return print_line(&json!({ "listening": false, "result": error_name(&e) })),
    };
    if print_line(&json!({ "listening": true, "port": port })) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    // Accept and close until stopped.
    for conn in listener.incoming() {
        drop(conn);
    }
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------------------------
// File system
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct FsArgs {
    absent: Vec<String>,
    read: Option<String>,
    ro: Option<String>,
    rw: Option<(String, String)>,
}

fn parse_fs_args(args: &[String]) -> Option<FsArgs> {
    let mut out = FsArgs::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--absent" => out.absent.push(it.next()?.clone()),
            "--read" => out.read = Some(it.next()?.clone()),
            "--ro" => out.ro = Some(it.next()?.clone()),
            "--rw" => out.rw = Some((it.next()?.clone(), it.next()?.clone())),
            _ => return None,
        }
    }
    Some(out)
}

fn fs_outside_mounts(args: &[String]) -> Option<Value> {
    let a = parse_fs_args(args)?;
    let absent: Vec<Value> = a
        .absent
        .iter()
        .map(|p| json!({ "path": p, "result": result_name(&std::fs::symlink_metadata(p).map(drop)) }))
        .collect();
    let read = a.read.map(|p| match std::fs::read_to_string(&p) {
        Ok(content) => json!({ "path": p, "result": "ok", "content": content }),
        Err(e) => json!({ "path": p, "result": error_name(&e) }),
    });
    let ro = a.ro.map(|d| {
        let path = format!("{d}/.probe-ro");
        let r = std::fs::write(&path, b"x");
        json!({ "path": path, "result": result_name(&r) })
    });
    let rw = a.rw.map(|(p, nonce)| {
        let r = std::fs::write(&p, nonce.as_bytes());
        json!({ "path": p, "result": result_name(&r) })
    });
    Some(json!({ "absent": absent, "read": read, "ro_write": ro, "rw_write": rw }))
}

// ---------------------------------------------------------------------------------------------
// Tree kill
// ---------------------------------------------------------------------------------------------

/// Start `crabd probe tree-kill-child <nonce>` (a new session, survives `SIGTERM`), wait until
/// it runs, print its pid, then wait to be stopped.
fn tree_kill(nonce: &str) -> ExitCode {
    let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
    let child = std::process::Command::new(exe)
        .args(["probe", "tree-kill-child", nonce])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return print_line(&json!({ "ready": false, "result": error_name(&e) })),
    };
    let mut line = String::new();
    if let Some(out) = child.stdout.take() {
        let _ = BufReader::new(out).read_line(&mut line);
    }
    let ready = line.trim() == "ready";
    if print_line(&json!({ "ready": ready, "child_pid": child.id() })) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// The grandchild: its own session (out of the program's process group) and a `SIGTERM`
/// handler that ignores the signal, so only the supervisor's forced kill of the tree stops it.
fn tree_kill_child() -> ExitCode {
    let _ = rustix::process::setsid();
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return ExitCode::FAILURE;
    };
    rt.block_on(async {
        let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return ExitCode::FAILURE;
        };
        println!("ready");
        let _ = io::stdout().flush();
        loop {
            term.recv().await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_names() {
        let e = |n| io::Error::from_raw_os_error(n);
        use rustix::io::Errno;
        assert_eq!(
            error_name(&e(Errno::NETUNREACH.raw_os_error())),
            "ENETUNREACH"
        );
        assert_eq!(error_name(&e(Errno::ROFS.raw_os_error())), "EROFS");
        assert_eq!(error_name(&e(Errno::AGAIN.raw_os_error())), "timeout");
        assert_eq!(error_name(&io::ErrorKind::TimedOut.into()), "timeout");
        assert_eq!(result_name(&Ok(())), "ok");
    }

    #[test]
    fn resolv_conf() {
        let got = resolvers(
            "# x\nnameserver 127.0.0.53\nnameserver fe80::1%eth0\noptions edns0\nnameserver bad\n",
        );
        assert_eq!(
            got,
            vec![
                "127.0.0.53:53".parse().unwrap(),
                "[fe80::1]:53".parse().unwrap()
            ]
        );
    }

    #[test]
    fn fs_args() {
        let a: Vec<String> = [
            "--absent", "/a", "--absent", "/b", "--read", "/r", "--ro", "/d",
        ]
        .into_iter()
        .chain(["--rw", "/w", "n"])
        .map(String::from)
        .collect();
        let p = parse_fs_args(&a).unwrap();
        assert_eq!(p.absent, ["/a", "/b"]);
        assert_eq!(p.rw, Some(("/w".into(), "n".into())));
        assert!(parse_fs_args(&["--rw".into(), "/w".into()]).is_none());
        assert!(parse_fs_args(&["--what".into()]).is_none());
    }

    #[test]
    fn probes_print_one_line() {
        // Off Linux these read nothing useful, but they must not panic.
        let v = tcp_attempts(&["127.0.0.1:1"]);
        assert_eq!(v["attempts"].as_array().unwrap().len(), 1);
        assert!(with_nonce("no-such-nonce-xyz-123").is_empty());
    }
}
