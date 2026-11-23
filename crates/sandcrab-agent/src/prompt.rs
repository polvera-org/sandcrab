//! The system prompt: concise, environment-specific, stable for the whole session (so it
//! stays in the prompt cache).

use std::fmt::Write as _;

/// One mounted workspace as the model should see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub guest_path: String,
    pub read_only: bool,
    /// e.g. "persistent workspace `landing`" or "host directory docs/site".
    pub description: String,
}

#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub template: String,
    pub cwd: String,
    pub mounts: Vec<MountInfo>,
    /// `disabled`, `unrestricted`, ...
    pub network: String,
    /// `(guest port, host address)`.
    pub ports: Vec<(u16, String)>,
    pub tools: Vec<String>,
}

#[must_use]
pub fn system_prompt(ctx: &PromptContext) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "You are crabagent, a coding agent working inside an isolated Linux sandbox \
         (Sandcrab template `{}`). You act only through the tools listed below; every tool \
         runs inside the sandbox, never on the user's machine.",
        ctx.template
    );
    s.push_str("\n# Environment\n");
    let _ = writeln!(
        s,
        "- Working directory: {} (relative paths resolve against it).",
        ctx.cwd
    );
    if ctx.mounts.is_empty() {
        s.push_str(
            "- No workspace is mounted: everything you write is discarded when the session ends.\n",
        );
    } else {
        s.push_str("- Mounts:\n");
        for m in &ctx.mounts {
            let _ = writeln!(
                s,
                "  - {}  {}  ({})",
                m.guest_path,
                if m.read_only {
                    "READ-ONLY: never write here; copy what you need elsewhere"
                } else {
                    "read-write, persists after this session"
                },
                m.description
            );
        }
        s.push_str("- Everything outside the read-write mounts is ephemeral.\n");
    }
    let net = match ctx.network.as_str() {
        "disabled" => {
            "disabled: no internet access, so package installs from registries will fail; \
                       use what the template provides"
                .to_owned()
        }
        "unrestricted" => "unrestricted (package installs work)".to_owned(),
        other => other.to_owned(),
    };
    let _ = writeln!(s, "- Network: {net}.");
    if !ctx.ports.is_empty() {
        let list = ctx
            .ports
            .iter()
            .map(|(g, h)| format!("{g} (user opens http://{h})"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            s,
            "- Published ports: {list}. Servers must listen on 0.0.0.0 on these exact ports \
             (e.g. `vite --host 0.0.0.0 --port <port> --strictPort`) to be reachable."
        );
    }
    let _ = writeln!(s, "\n# Tools\n{}", ctx.tools.join(", "));
    s.push_str(
        "\n# How to work\n\
         - Look before you change things: list_dir, glob, grep and file_read are cheap.\n\
         - Edit existing files with file_edit (exact old_string → new_string, with enough \
         context to be unique). Use file_write only for new files or full rewrites.\n\
         - Use bash for builds, tests, package managers and git. It has a timeout: never use it \
         for servers or watchers.\n\
         - Long-running processes (dev servers, watchers): start them with process_start, then \
         check their output with process_poll; stop them with process_kill. Leave a requested \
         dev server running when you finish.\n\
         - Independent tool calls can be issued together in one turn; they run in parallel.\n\
         - Verify your work: build, run tests, poll the server, read back what you wrote. Fix \
         failures before you finish.\n\
         - Keep going until the task is done, then reply with a short summary: what changed, \
         where, and how to see it.\n\
         - Keep prose brief; let the tool calls do the work.\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_everything() {
        let p = system_prompt(&PromptContext {
            template: "ts-dev".into(),
            cwd: "/workspace".into(),
            mounts: vec![
                MountInfo {
                    guest_path: "/workspace".into(),
                    read_only: false,
                    description: "persistent workspace `landing`".into(),
                },
                MountInfo {
                    guest_path: "/kit".into(),
                    read_only: true,
                    description: "host directory docs/site".into(),
                },
            ],
            network: "unrestricted".into(),
            ports: vec![(5173, "127.0.0.1:40001".into())],
            tools: vec!["bash".into(), "file_edit".into(), "process_start".into()],
        });
        for needle in [
            "ts-dev",
            "/workspace",
            "/kit  READ-ONLY",
            "unrestricted",
            "5173",
            "0.0.0.0",
            "file_edit",
            "process_start",
            "process_poll",
            "Verify",
        ] {
            assert!(p.contains(needle), "missing {needle}:\n{p}");
        }
        let p = system_prompt(&PromptContext {
            network: "disabled".into(),
            ..PromptContext::default()
        });
        assert!(p.contains("no internet"));
        assert!(p.contains("No workspace is mounted"));
    }
}
