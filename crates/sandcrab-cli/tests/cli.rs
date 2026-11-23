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
                .env("SANDCRAB_ALLOW_INSECURE", "1")
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
fn backend_must_be_explicit_and_insecure_opted_in() {
    let cli = Cli::new(false);
    let out = cli.run(&["run", "--", "true"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no backend selected"),
        "{}",
        stderr(&out)
    );

    let out = cli.run(&["--backend", "docker", "run", "--", "true"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("--allow-insecure"),
        "{}",
        stderr(&out)
    );

    let out = cli.run(&["--backend", "firecracker", "run", "--", "true"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not compiled"), "{}", stderr(&out));
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

    // cat a workspace file; insecure warning + timings on stderr
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
    assert!(err.contains("shares the host kernel"), "{err}");
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
