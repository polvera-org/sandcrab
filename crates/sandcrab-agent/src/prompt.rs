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
    /// One-shot (`-p`) session: the sandbox is destroyed when the task ends.
    pub one_shot: bool,
    /// `--keep-alive` seconds the sandbox is held after a one-shot task (0 = none).
    pub keep_alive_secs: u64,
    /// Package/browser cache dir inside a Sandcrab-managed workspace, if one was set up
    /// (never inside a host directory).
    pub cache_dir: Option<String>,
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
        if ctx.one_shot && ctx.keep_alive_secs == 0 {
            s.push_str(
                "- This is a one-shot session: the sandbox and every server in it are destroyed \
                 as soon as you finish, so the URL above will be dead by the time the user reads \
                 your reply. Don't present it as live; say how to preview the result instead \
                 (the built files in the workspace, or the command to serve them).\n",
            );
        } else if ctx.one_shot {
            let _ = writeln!(
                s,
                "- After you finish, the sandbox stays up for {}s so the user can open the URL; \
                 leave the server running.",
                ctx.keep_alive_secs
            );
        }
    }
    s.push_str(
        "- `/tmp` may be mounted noexec: never run binaries or load shared libraries from it. \
         Unpack tools under $HOME or the workspace instead (e.g. `export TMPDIR=$HOME/.tmp`).\n\
         - $HOME and everything outside the read-write mounts is reset every session.\n",
    );
    if let Some(dir) = &ctx.cache_dir {
        let _ = writeln!(
            s,
            "- Package and browser caches (npm, pip/uv, Playwright via XDG_CACHE_HOME) live in \
             {dir}, inside the persistent workspace, so installs are fast next session. Keep \
             it out of git (.gitignore) and out of build output."
        );
    }
    if ctx.template == "render" {
        s.push_str(
            "- Headless Chromium + Playwright are preinstalled (`require('playwright')`, \
             `chromium` on PATH). Take screenshots and look at them with file_read.\n",
        );
    } else {
        s.push_str(
            "- No headless browser is installed here. Chromium + Playwright come preinstalled \
             only in the `render` template; installing one in this template is slow and needs \
             system libraries it lacks. Unless the user asks for it, verify visual work another \
             way (build output, served HTML) and suggest a `--template render` session for \
             screenshots.\n",
        );
    }
    let _ = writeln!(
        s,
        "- file_read or download of a PNG/JPEG/GIF/WebP image shows you the image itself (up \
         to ~3.75 MB and {} px per side), so you can inspect screenshots and assets. Take \
         viewport-sized screenshots, not full-page ones. Other binary files, and images that \
         are too large or truncated, come back as a summary (size, sha256, type), never as \
         base64. Only the last few images stay in the conversation; older ones become \
         `[image elided: …]` notes, so write down what you saw that matters.",
        crate::media::MAX_IMAGE_SIDE
    );
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
            one_shot: true,
            keep_alive_secs: 0,
            cache_dir: Some("/workspace/.cache".into()),
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
            "one-shot session",
            "noexec",
            "/workspace/.cache",
            "`render` template",
            "image itself",
            "8000 px",
            "image elided",
        ] {
            assert!(p.contains(needle), "missing {needle}:\n{p}");
        }
        let p = system_prompt(&PromptContext {
            network: "disabled".into(),
            ..PromptContext::default()
        });
        assert!(p.contains("no internet"));
        assert!(p.contains("No workspace is mounted"));
        assert!(!p.contains("one-shot"), "no ports → no preview warning");

        let p = system_prompt(&PromptContext {
            template: "render".into(),
            ports: vec![(8080, "127.0.0.1:1".into())],
            one_shot: true,
            keep_alive_secs: 60,
            ..PromptContext::default()
        });
        assert!(p.contains("stays up for 60s"), "{p}");
        assert!(p.contains("preinstalled (`require('playwright')`"), "{p}");
        assert!(!p.contains("No headless browser"));
    }
}
