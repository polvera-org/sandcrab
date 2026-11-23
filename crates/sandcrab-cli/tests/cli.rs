//! CLI tests. The Docker smoke tests are `#[ignore]`d; run them with:
//!
//! ```text
//! templates/build.sh --pin        # sandcrab/base:0.1.0 present
//! cargo test -p sandcrab-cli -- --ignored --nocapture
//! ```
//!
//! `crabd` comes from `$SANDCRAB_CRABD`, else `target/<arch>-unknown-linux-musl/release/
//! crabd`, built with `scripts/build-crabd.sh` on first use if missing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Path of a static crabd, building it once if needed.
fn crabd() -> &'static Path {
    static CRABD: OnceLock<PathBuf> = OnceLock::new();
    CRABD.get_or_init(|| {
        if let Some(p) = std::env::var_os("SANDCRAB_CRABD").filter(|v| !v.is_empty()) {
            return PathBuf::from(p);
        }
        let default = root().join(format!(
            "target/{}-unknown-linux-musl/release/crabd",
            std::env::consts::ARCH
        ));
        if default.is_file() {
            return default;
        }
        eprintln!("building static crabd via scripts/build-crabd.sh ...");
        let out = Command::new(root().join("scripts/build-crabd.sh"))
            .current_dir(root())
            .output()
            .expect("run scripts/build-crabd.sh");
        assert!(
            out.status.success(),
            "build-crabd.sh failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("path:").map(|p| PathBuf::from(p.trim())))
            .expect("build-crabd.sh prints the binary path")
    })
}

struct Cli {
    state: tempfile::TempDir,
    docker: bool,
}

impl Cli {
    fn new(docker: bool) -> Self {
        Self {
            state: tempfile::tempdir().unwrap(),
            docker,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_sandcrab"));
        c.arg("--state-dir")
            .arg(self.state.path())
            .arg("--templates-dir")
            .arg(root().join("templates"))
            .args(args)
            .env_remove("SANDCRAB_BACKEND")
            .env_remove("SANDCRAB_ALLOW_INSECURE")
            .env_remove("SANDCRAB_HOME");
        if self.docker {
            c.env("SANDCRAB_BACKEND", "docker")
                .env("SANDCRAB_CRABD", crabd());
        }
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        let out = self.cmd(args).output().unwrap();
        eprintln!(
            "$ sandcrab {}\n  -> {:?}\n  stdout: {}\n  stderr: {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim_end(),
            String::from_utf8_lossy(&out.stderr).trim_end()
        );
        out
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "sandcrab {args:?} failed");
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn workspace_commands_without_backend() {
    let cli = Cli::new(false);
    let ws = cli.json(&["workspace", "create", "demo"]);
    assert_eq!(ws["name"], "demo");

    let src = cli.state.path().join("local.txt");
    std::fs::write(&src, "from file\n").unwrap();
    cli.ok(&["workspace", "put", "demo", "a/b.txt", src.to_str().unwrap()]);

    let mut child = cli
        .cmd(&["workspace", "put", "demo", "stdin.txt", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from stdin")
        .unwrap();
    assert!(child.wait().unwrap().success());

    assert_eq!(
        cli.ok(&["workspace", "cat", "demo", "a/b.txt"]),
        "from file\n"
    );
    assert_eq!(
        cli.ok(&["workspace", "cat", "demo", "stdin.txt"]),
        "from stdin"
    );
    let files = cli.json(&["workspace", "files", "demo"]);
    assert_eq!(files.as_array().unwrap().len(), 2);
    assert!(cli.ok(&["workspace", "ls"]).contains("demo"));

    let escape = cli.run(&["workspace", "cat", "demo", "../../etc/passwd"]);
    assert_eq!(escape.status.code(), Some(1));

    cli.ok(&["workspace", "rm", "demo", "--delete-data"]);
    assert_eq!(cli.json(&["workspace", "ls"]), Value::Array(vec![]));
}

#[test]
fn templates_and_schema() {
    let cli = Cli::new(false);
    let list = cli.json(&["template", "list"]);
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"base"), "{names:?}");

    let schema = cli.json(&["tools", "schema"]);
    let tools = schema.as_array().unwrap();
    assert_eq!(tools.len(), 16);
    assert!(tools.iter().any(|t| t["name"] == "glob"));
}

#[test]
fn backend_must_be_explicit_and_docker_needs_no_opt_in() {
    let cli = Cli::new(false);
    // No default backend: never picks docker (or anything else) for you.
    let out = cli.run(&["run", "--", "true"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no backend selected"),
        "{}",
        stderr(&out)
    );

    // Docker no longer asks for an opt-in: it gets as far as checking crabd.
    let out = cli.run(&[
        "--backend",
        "docker",
        "--crabd",
        "/nonexistent/crabd",
        "run",
        "--",
        "true",
    ]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("crabd"), "{err}");
    assert!(!err.contains("allow-insecure"), "{err}");
    assert!(!err.contains("local dev"), "{err}");

    let out = cli.run(&["--backend", "libkrun", "run", "--", "true"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not compiled"), "{}", stderr(&out));
}

/// `--allow-insecure` / `SANDCRAB_ALLOW_INSECURE` are deprecated no-ops: still accepted
/// (hidden from --help), they print a warning and change nothing.
#[test]
fn deprecated_allow_insecure_is_a_noop_with_warning() {
    let cli = Cli::new(false);
    let help = String::from_utf8_lossy(&cli.run(&["--help"]).stdout).into_owned();
    assert!(!help.contains("allow-insecure"), "{help}");

    let plain = cli.run(&["workspace", "list"]);
    assert_eq!(plain.status.code(), Some(0));
    assert!(!stderr(&plain).contains("deprecated"), "{}", stderr(&plain));

    let flag = cli.run(&["--allow-insecure", "workspace", "list"]);
    let env = cli
        .cmd(&["workspace", "list"])
        .env("SANDCRAB_ALLOW_INSECURE", "1")
        .output()
        .unwrap();
    for out in [&flag, &env] {
        let err = stderr(out);
        assert_eq!(out.status.code(), Some(0), "{err}");
        assert!(err.contains("deprecated and has no effect"), "{err}");
        assert_eq!(out.stdout, plain.stdout, "output changed");
    }

    // Still a no-op for backend selection: no backend → still an error.
    let out = cli.run(&["--allow-insecure", "run", "--", "true"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no backend selected"));
}

fn kvm_usable() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

/// Sandcrab-managed Docker containers on this host (`None` without a Docker daemon).
fn docker_sandboxes() -> Option<usize> {
    let out = Command::new("docker")
        .args(["ps", "-aq", "--filter", "label=sandcrab.managed=true"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).lines().count())
}

/// `--backend firecracker` is hardware isolated and, without KVM, fails
/// with an actionable error: no fallback to docker, no container created.
#[cfg(target_os = "linux")]
#[test]
fn firecracker_without_kvm_is_a_clear_error() {
    if kvm_usable() {
        eprintln!("skipped: /dev/kvm is usable on this host");
        return;
    }
    let cli = Cli::new(false);
    let before = docker_sandboxes();
    // Even with the (deprecated) docker opt-in present, an explicit firecracker selection
    // never degrades to docker.
    let out = cli
        .cmd(&["--backend", "firecracker", "run", "--", "echo", "hi"])
        .env("SANDCRAB_ALLOW_INSECURE", "1")
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(out.stdout.is_empty());
    assert!(err.contains("firecracker backend unavailable"), "{err}");
    assert!(err.contains("/dev/kvm"), "{err}");
    assert!(err.contains("usermod -aG kvm"), "{err}");
    assert!(err.contains("never falls back"), "{err}");
    assert!(!err.contains("backend `docker`"), "{err}");
    assert!(err.contains("deprecated and has no effect"), "{err}");
    assert_eq!(docker_sandboxes(), before, "a docker container was created");

    // Same through SANDCRAB_BACKEND and the `fc` alias.
    let out = cli
        .cmd(&["tool", "glob"])
        .env("SANDCRAB_BACKEND", "fc")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("/dev/kvm"), "{}", stderr(&out));
}

/// `doctor` reports the Firecracker prerequisites; on a host without KVM the summary says
/// so, and selecting firecracker turns it into a failing check.
#[cfg(target_os = "linux")]
#[test]
fn firecracker_doctor_section() {
    let cli = Cli::new(false);
    let checks = |extra: &[&str]| {
        let mut args = vec!["--json"];
        args.extend_from_slice(extra);
        args.push("doctor");
        let out = cli.run(&args);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        (out.status.code(), v)
    };
    let find = |v: &Value, name: &str| {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no `{name}` check in {v}"))
    };
    let (_, v) = checks(&[]);
    for name in ["fc:kvm", "fc:kvm_group", "fc:mkfs", "fc:crabd", "fc:taps"] {
        find(&v, name);
    }
    let summary = find(&v, "firecracker");
    let detail = summary["detail"].as_str().unwrap();
    assert!(detail.contains("e2e pending"), "{detail}");
    if !kvm_usable() {
        assert!(
            detail.starts_with("Firecracker: unavailable (") && detail.contains("/dev/kvm"),
            "{detail}"
        );
        assert_eq!(summary["status"], "info");
        let (code, v) = checks(&["--backend", "firecracker"]);
        assert_eq!(code, Some(1));
        assert_eq!(find(&v, "firecracker")["status"], "fail");
        assert_eq!(find(&v, "fc:kvm")["status"], "fail");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn rootfs_commands_parse() {
    let cli = Cli::new(false);
    let out = cli.run(&["rootfs", "build"]);
    assert_eq!(out.status.code(), Some(2), "names or --all required");
    // Missing crabd is reported before any Docker work.
    let out = cli.run(&["--crabd", "/nonexistent/crabd", "rootfs", "build", "base"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("crabd"), "{}", stderr(&out));
    let v = cli.json(&["rootfs", "list"]);
    assert_eq!(v, Value::Array(vec![]));
}

/// A template without a `sha256` pin is refused by default; `--allow-unpinned` opts out.
#[test]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
fn docker_unpinned_template_is_refused_by_default() {
    let cli = Cli::new(true);
    let dir = cli.state.path().join("tpl/unpinned");
    std::fs::create_dir_all(&dir).unwrap();
    let base = std::fs::read_to_string(root().join("templates/base/sandcrab.toml")).unwrap();
    let manifest: String = base
        .lines()
        .filter(|l| !l.starts_with("sha256"))
        .map(|l| {
            if l.starts_with("name = ") {
                "name = \"unpinned\"\n".to_owned()
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    std::fs::write(dir.join("sandcrab.toml"), manifest).unwrap();
    let tpl = cli.state.path().join("tpl");
    let tpl = tpl.to_str().unwrap();

    let out = cli.run(&[
        "--templates-dir",
        tpl,
        "run",
        "-t",
        "unpinned",
        "--",
        "true",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("UnpinnedImage"), "{err}");
    assert!(err.contains("templates/build.sh --pin unpinned"), "{err}");

    let out = cli.run(&[
        "--templates-dir",
        tpl,
        "--allow-unpinned",
        "run",
        "-t",
        "unpinned",
        "--",
        "true",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
fn docker_smoke_run_and_tool() {
    let cli = Cli::new(true);
    cli.ok(&["workspace", "create", "demo"]);
    let src = cli.state.path().join("hello.txt");
    std::fs::write(&src, "hello from the host\n").unwrap();
    cli.ok(&[
        "workspace",
        "put",
        "demo",
        "hello.txt",
        src.to_str().unwrap(),
    ]);

    // cat a workspace file; disk_mb warning + timings on stderr (no dev-only warning)
    let out = cli.run(&[
        "-v",
        "run",
        "--mount",
        "demo:/workspace",
        "--",
        "cat",
        "hello.txt",
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from the host\n"
    );
    let err = stderr(&out);
    assert!(!err.contains("shares the host kernel"), "{err}");
    assert!(!err.contains("deprecated"), "{err}");
    assert!(err.contains("disk_mb is not enforced"), "{err}");
    assert_eq!(err.matches("disk_mb").count(), 1, "warned twice: {err}");
    assert!(
        err.contains("ready in") && err.contains("teardown"),
        "{err}"
    );

    // exit code propagation
    let out = cli.run(&["run", "--", "bash", "-c", "exit 3"]);
    assert_eq!(out.status.code(), Some(3));

    // stderr streaming + writes persist in the rw workspace
    let out = cli.run(&[
        "run",
        "-m",
        "demo:/workspace",
        "--",
        "echo out; echo err >&2; echo made > made.txt",
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    assert!(stderr(&out).contains("err"));
    assert_eq!(cli.ok(&["workspace", "cat", "demo", "made.txt"]), "made\n");

    // read-only mount refuses writes
    let out = cli.run(&["run", "-m", "demo:/data:ro", "--", "touch /data/x"]);
    assert_ne!(out.status.code(), Some(0));

    // --json summary
    let v = cli.json(&["run", "-e", "GREETING=hi", "--", "echo $GREETING"]);
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["stdout"], "hi\n");
    assert!(v["timings"]["spawn_ready_ms"].as_f64().unwrap() > 0.0);

    // one-off tool call
    let v = cli.json(&[
        "tool",
        "--mount",
        "demo:/workspace",
        "glob",
        r#"{"pattern":"**/*.txt"}"#,
    ]);
    let paths: Vec<&str> = v["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"hello.txt") && paths.contains(&"made.txt"),
        "{paths:?}"
    );

    // tool errors are structured JSON + exit 1
    let out = cli.run(&["tool", "file_read", r#"{"path":"/nope"}"#]);
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "not_found");
    let out = cli.run(&["tool", "nope"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "unknown_tool");

    // nothing left running: workspace is no longer leased and can be removed
    cli.ok(&["workspace", "rm", "demo", "--delete-data"]);
}

#[test]
#[ignore = "needs Docker and a static crabd"]
fn docker_doctor() {
    let cli = Cli::new(true);
    let out = cli.run(&["--json", "doctor"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let status = |name: &str| {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| c["status"].as_str().unwrap().to_owned())
    };
    assert_eq!(status("crabd").as_deref(), Some("ok"));
    assert_eq!(status("docker").as_deref(), Some("ok"));
    assert!(status("template:base").is_some());
    assert!(status("firecracker").is_some());
}

/// Containers (running or not) labeled with `sandcrab.sandbox_id=<id>`.
fn containers_of(sandbox_id: &str) -> usize {
    let out = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=sandcrab.sandbox_id={sandbox_id}"),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "docker ps failed");
    String::from_utf8_lossy(&out.stdout).lines().count()
}

/// Start `sandcrab -v run -- sleep 100` in the background; returns it once its sandbox is
/// up, with the sandbox id.
fn spawn_sleeper(cli: &Cli) -> (std::process::Child, String) {
    use std::io::{BufRead, BufReader};

    let mut child = cli
        .cmd(&["-v", "run", "--", "sleep", "100"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("  stderr: {line}");
            // "sandcrab: sandbox <id> (docker, base) ready in N ms"
            if let Some(rest) = line.strip_prefix("sandcrab: sandbox ")
                && line.contains("ready in")
            {
                let _ = tx.send(rest.split_whitespace().next().unwrap_or("").to_owned());
            }
        }
    });
    let id = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("sandbox did not come up");
    assert_eq!(containers_of(&id), 1, "sandbox {id} container not found");
    // Let the command start inside the sandbox.
    std::thread::sleep(std::time::Duration::from_millis(500));
    (child, id)
}

fn kill(sig: &str, child: &std::process::Child) {
    let st = Command::new("kill")
        .args([&format!("-{sig}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(st.success());
}

/// `sandcrab -v run -- sleep 100`, then `kill -<sig>` once the sandbox is up: the sandbox
/// must be destroyed and `sandcrab` must exit `128 + signal`.
fn signal_tears_down(sig: &str, num: i32) {
    let cli = Cli::new(true);
    let (mut child, id) = spawn_sleeper(&cli);
    kill(sig, &child);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sandcrab did not exit after SIG{sig}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(128 + num), "SIG{sig}: {status:?}");
    assert_eq!(
        containers_of(&id),
        0,
        "SIG{sig} leaked the container of sandbox {id}"
    );
}

/// `doctor --fix` must not touch a live sandbox of another sandcrab process (same user,
/// same or other state dir), and must reap it once that process is gone (SIGKILL: no
/// cleanup ran).
#[test]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
fn docker_doctor_fix_spares_live_sandboxes_of_other_processes() {
    let cli = Cli::new(true);
    let other_home = Cli::new(true);
    let (mut child, id) = spawn_sleeper(&cli);

    cli.run(&["doctor", "--fix"]);
    assert_eq!(containers_of(&id), 1, "doctor --fix reaped a live sandbox");
    other_home.run(&["doctor", "--fix"]);
    assert_eq!(
        containers_of(&id),
        1,
        "doctor --fix of another state dir reaped it"
    );
    // --all without confirmation (no terminal, no --yes) removes nothing.
    let out = other_home.run(&["doctor", "--fix", "--all"]);
    assert!(stderr(&out).contains("WARNING"), "{}", stderr(&out));
    assert_eq!(containers_of(&id), 1, "unconfirmed --all reaped it");

    kill("KILL", &child);
    child.wait().unwrap();
    assert_eq!(
        containers_of(&id),
        1,
        "expected a leaked container after SIGKILL"
    );
    other_home.run(&["doctor", "--fix"]);
    assert_eq!(containers_of(&id), 1, "another state dir reaped our orphan");
    cli.run(&["doctor", "--fix"]);
    assert_eq!(containers_of(&id), 0, "orphan of a dead process not reaped");
}

#[test]
fn doctor_all_requires_fix() {
    let cli = Cli::new(false);
    assert_eq!(cli.run(&["doctor", "--all"]).status.code(), Some(2));
    assert_eq!(
        cli.run(&["doctor", "--fix", "--yes"]).status.code(),
        Some(2)
    );
}

#[test]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
fn docker_run_sigterm_destroys_the_sandbox() {
    signal_tears_down("TERM", 15);
}

#[test]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
fn docker_run_sighup_destroys_the_sandbox() {
    signal_tears_down("HUP", 1);
}

#[test]
#[ignore = "needs Docker, sandcrab/base:0.1.0 and a static crabd"]
fn docker_run_sigquit_destroys_the_sandbox() {
    signal_tears_down("QUIT", 3);
}
