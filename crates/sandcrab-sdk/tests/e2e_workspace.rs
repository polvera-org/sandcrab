//! Persistent-workspace e2e scenarios (BRIEF.md §2, AGENTS.md §1.4): multi-turn host
//! mutation with a cross-template handoff, multi-mount rw/ro + `default_cwd`, and host-side
//! path safety. See `tests/common/mod.rs` for how to run the suite.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::{run, sh};
use sandcrab_sdk::{MountMode, ProtocolError, SandboxConfig, SandboxError};
use serde_json::Value;

const TURN1_PY: &str = r#"
import json
json.dump({"turn": 1, "values": [1, 2, 3]}, open("data.json", "w"))
open("report.py", "w").write(
    "import json\nprint(sum(json.load(open('data.json'))['values']))\n"
)
open("scratch.tmp", "w").write("delete me\n")
open("notes.md", "w").write("turn 1\n")
"#;

fn names(entries: &[sandcrab_sdk::FileEntry]) -> BTreeSet<String> {
    entries
        .iter()
        .map(|e| {
            e.path
                .file_name()
                .unwrap_or(e.path.as_os_str())
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn multi_turn_workspace_mutation_cross_template() {
    run("multi-turn", |h| async move {
        let ws = h.ws();
        let name = h.name("proj");
        ws.create(&name).unwrap();

        // ---- Turn 1: python-dev writes outputs -------------------------------------
        let t1 =
            h.sc.spawn(
                SandboxConfig::from_template("python-dev")
                    .mount(name.as_str(), "/workspace", MountMode::ReadWrite)
                    .default_cwd("/workspace"),
            )
            .await
            .unwrap();
        let t1_id = t1.id();
        // Host mutation is refused while a sandbox holds the workspace read-write.
        let busy = ws.put_file(name.as_str(), "early.txt", "x").unwrap_err();
        assert!(matches!(busy, SandboxError::WorkspaceBusy(_)), "{busy}");

        t1.file_write("turn1.py", TURN1_PY).await.unwrap();
        assert_eq!(
            sh(&t1, "python3 turn1.py && python3 report.py").await,
            "6\n"
        );
        t1.file_delete("turn1.py", false).await.unwrap();
        t1.destroy().await.unwrap();

        // ---- Between turns: nothing running, host mutates ------------------------------
        h.assert_none_running(&[t1_id]);
        let data: Value =
            serde_json::from_slice(&ws.read_file(name.as_str(), "data.json").unwrap()).unwrap();
        assert_eq!(data["turn"], 1);
        assert_eq!(data["values"], serde_json::json!([1, 2, 3]));
        ws.put_file(name.as_str(), "inbox/attachment.txt", "from the host\n")
            .unwrap();
        ws.delete_file(name.as_str(), "scratch.tmp").unwrap();
        assert_eq!(
            names(&ws.list_files(name.as_str(), "").unwrap()),
            set(&["data.json", "inbox", "notes.md", "report.py"])
        );

        // ---- Turn 2: a different template sees turn-1 outputs + host changes -----------
        let t2 =
            h.sc.spawn(
                SandboxConfig::from_template("base")
                    .mount(name.as_str(), "/workspace", MountMode::ReadWrite)
                    .default_cwd("/workspace"),
            )
            .await
            .unwrap();
        let t2_id = t2.id();
        assert_ne!(t1_id, t2_id);
        assert_eq!(t2.template(), "base");
        assert_eq!(sh(&t2, "cat inbox/attachment.txt").await, "from the host\n");
        assert_eq!(
            sh(&t2, "test ! -e scratch.tmp && echo gone").await,
            "gone\n"
        );
        let missing = t2.file_read("scratch.tmp").await.unwrap_err();
        assert!(
            matches!(missing, SandboxError::Protocol(ProtocolError::NotFound(_))),
            "{missing}"
        );
        let read = t2.file_read("data.json").await.unwrap();
        assert!(read.content.contains("\"turn\": 1"), "{}", read.content);
        let report = t2.file_read("report.py").await.unwrap();
        assert!(report.content.contains("sum("), "{}", report.content);
        let mut listed: Vec<String> = t2
            .list_dir(None)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.path.trim_end_matches('/').to_owned())
            .collect();
        listed.sort();
        assert!(
            listed.iter().all(|p| !p.ends_with("scratch.tmp")),
            "{listed:?}"
        );

        t2.file_write_with(sandcrab_sdk::FileWriteParams {
            path: "notes.md".into(),
            content: "turn 2\n".into(),
            append: true,
        })
        .await
        .unwrap();
        sh(&t2, "echo 'turn 2 (bash)' >> notes.md").await;
        t2.file_write("inbox/reply.txt", "ack\n").await.unwrap();
        t2.destroy().await.unwrap();

        // ---- Host verifies the final state ----------------------------------------
        h.assert_none_running(&[t1_id, t2_id]);
        assert_eq!(
            ws.read_file(name.as_str(), "notes.md").unwrap(),
            b"turn 1\nturn 2\nturn 2 (bash)\n"
        );
        assert_eq!(
            ws.read_file(name.as_str(), "inbox/reply.txt").unwrap(),
            b"ack\n"
        );
        assert_eq!(
            names(&ws.list_files(name.as_str(), "inbox").unwrap()),
            set(&["attachment.txt", "reply.txt"])
        );
        assert_eq!(
            names(&ws.list_files(name.as_str(), "").unwrap()),
            set(&["data.json", "inbox", "notes.md", "report.py"])
        );
        // Mutable again once no sandbox holds it.
        ws.put_file(name.as_str(), "done.txt", "ok").unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn multi_mount_rw_ro_and_default_cwd() {
    run("multi-mount", |h| async move {
        let ws = h.ws();
        let a = h.name("a");
        let b = h.name("b");
        ws.create(&a).unwrap();
        ws.create(&b).unwrap();
        ws.put_file(b.as_str(), "readme.txt", "read only\n")
            .unwrap();

        let sb =
            h.sc.spawn(
                SandboxConfig::from_template("base")
                    .mount(a.as_str(), "/workspace", MountMode::ReadWrite)
                    .mount(b.as_str(), "/data", MountMode::ReadOnly)
                    .default_cwd("/workspace"),
            )
            .await
            .unwrap();
        assert_eq!(sb.default_cwd(), Path::new("/workspace"));
        assert_eq!(sb.hello().default_cwd, "/workspace");
        assert_eq!(sh(&sb, "pwd").await, "/workspace\n");

        // Relative paths resolve against default_cwd → workspace A.
        sb.file_write("rel.txt", "relative\n").await.unwrap();
        sb.file_write("sub/dir/nested.txt", "nested\n")
            .await
            .unwrap();
        assert_eq!(sh(&sb, "cat /data/readme.txt").await, "read only\n");

        // Read-only mount: refused by the tool and by the kernel.
        let err = sb.file_write("/data/x", "nope").await.unwrap_err();
        assert!(matches!(err, SandboxError::Protocol(_)), "{err}");
        let touch = sb.bash("touch /data/x").await.unwrap();
        assert_ne!(touch.exit_code, Some(0), "{touch:?}");
        assert!(
            touch.stderr.contains("Read-only file system"),
            "expected EROFS: {touch:?}"
        );
        let rm = sb.bash("rm -f /data/readme.txt").await.unwrap();
        assert_ne!(rm.exit_code, Some(0), "{rm:?}");
        let err = sb.file_delete("/data/readme.txt", false).await.unwrap_err();
        assert!(matches!(err, SandboxError::Protocol(_)), "{err}");

        // A read-only lease does not block host-side mutation of B, a rw lease blocks A.
        assert!(matches!(
            ws.put_file(a.as_str(), "host.txt", "x").unwrap_err(),
            SandboxError::WorkspaceBusy(_)
        ));
        sb.destroy().await.unwrap();

        assert_eq!(ws.read_file(a.as_str(), "rel.txt").unwrap(), b"relative\n");
        assert_eq!(
            ws.read_file(a.as_str(), "sub/dir/nested.txt").unwrap(),
            b"nested\n"
        );
        assert_eq!(
            names(&ws.list_files(b.as_str(), "").unwrap()),
            set(&["readme.txt"])
        );
        assert_eq!(
            ws.read_file(b.as_str(), "readme.txt").unwrap(),
            b"read only\n"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "e2e: needs a sandbox backend (SANDCRAB_E2E_BACKEND, default docker)"]
async fn host_side_path_safety() {
    run("path-safety", |h| async move {
        let ws = h.ws();
        let name = h.name("ws");
        let created = ws.create(&name).unwrap();
        let root = created.backing.path().to_path_buf();
        let is_escape = |e: &SandboxError| {
            matches!(
                e,
                SandboxError::PathEscape(_) | SandboxError::InvalidConfig(_)
            )
        };

        // Lexical escapes.
        for bad in ["../escape", "a/../../escape", "/etc/sandcrab-e2e-abs"] {
            let err = ws.put_file(name.as_str(), bad, "x").unwrap_err();
            assert!(is_escape(&err), "put_file({bad}): {err}");
            let err = ws.read_file(name.as_str(), bad).unwrap_err();
            assert!(is_escape(&err), "read_file({bad}): {err}");
        }
        assert!(!root.parent().unwrap().join("escape").exists());
        assert!(!Path::new("/etc/sandcrab-e2e-abs").exists());

        // Symlink escapes planted by the sandbox.
        let canary = h.tmp().join("canary");
        std::fs::create_dir_all(&canary).unwrap();
        std::fs::write(canary.join("secret.txt"), "host secret\n").unwrap();
        let sb =
            h.sc.spawn(
                SandboxConfig::from_template("base")
                    .mount(name.as_str(), "/workspace", MountMode::ReadWrite)
                    .default_cwd("/workspace"),
            )
            .await
            .unwrap();
        sh(
            &sb,
            &format!(
                "ln -s / evil && ln -s /etc/passwd pw && ln -s {} canary && \
                 mkdir -p ok && echo fine > ok/real.txt && ln -s ok/real.txt inside && \
                 ln -s /nonexistent dangling",
                canary.display()
            ),
        )
        .await;
        sb.destroy().await.unwrap();

        for bad in ["evil/etc/passwd", "pw", "canary/secret.txt", "dangling"] {
            let err = ws.read_file(name.as_str(), bad).unwrap_err();
            assert!(
                matches!(err, SandboxError::PathEscape(_)),
                "read_file({bad}) must be refused: {err}"
            );
        }
        for bad in [
            "evil/tmp/sandcrab-e2e-pwned",
            "canary/pwned.txt",
            "dangling",
        ] {
            let err = ws.put_file(name.as_str(), bad, "pwned").unwrap_err();
            assert!(
                matches!(err, SandboxError::PathEscape(_)),
                "put_file({bad}) must be refused: {err}"
            );
        }
        assert!(!Path::new("/tmp/sandcrab-e2e-pwned").exists());
        assert!(!canary.join("pwned.txt").exists());
        let err = ws.list_files(name.as_str(), "evil").unwrap_err();
        assert!(matches!(err, SandboxError::PathEscape(_)), "{err}");

        // A symlink that stays inside the workspace is fine.
        assert_eq!(ws.read_file(name.as_str(), "inside").unwrap(), b"fine\n");

        // delete_file removes the link itself, never its target.
        ws.delete_file(name.as_str(), "canary").unwrap();
        ws.delete_file(name.as_str(), "evil").unwrap();
        assert_eq!(
            std::fs::read(canary.join("secret.txt")).unwrap(),
            b"host secret\n"
        );
        assert!(Path::new("/etc/passwd").exists());
    })
    .await;
}
