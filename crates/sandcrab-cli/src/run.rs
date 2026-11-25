//! `sandcrab run` and `sandcrab tool`.

use std::io::Write;
use std::time::Duration;

use anyhow::Context;
use sandcrab_sdk::{
    KillSignal, ProcessStartParams, Sandbox, SandboxConfig, Sandcrab, TimingMarks, is_tool_error,
};
use serde_json::{Value, json};

use crate::Global;
use crate::args::{SpawnArgs, shell_join};
use crate::util::{self, Interrupt, ms, print_json};

const POLL_MIN: Duration = Duration::from_millis(20);
const POLL_MAX: Duration = Duration::from_millis(250);

/// Spawn with Ctrl-C deferred until the sandbox is up (so it can be torn down cleanly).
async fn spawn(g: &Global, sc: &Sandcrab, config: SandboxConfig) -> anyhow::Result<Sandbox> {
    util::warn_unenforced_limits(sc);
    let template = config.template.clone();
    let sb = sc
        .spawn(config)
        .await
        .with_context(|| format!("spawning a `{template}` sandbox"))?;
    if g.verbose > 0 {
        eprintln!(
            "sandcrab: sandbox {} ({}, {}) ready in {} ms",
            sb.id(),
            sb.backend(),
            template,
            ms(sb.timings().spawn_ready())
        );
    }
    for p in sb.published_ports() {
        eprintln!("sandcrab: port {} -> {}", p.guest, p.host);
    }
    Ok(sb)
}

async fn destroy(g: &Global, sb: Sandbox) -> anyhow::Result<TimingMarks> {
    let id = sb.id();
    let marks = sb
        .destroy()
        .await
        .with_context(|| format!("destroying sandbox {id}"))?;
    if g.verbose > 0 {
        eprintln!(
            "sandcrab: sandbox {id} destroyed; spawn→ready {} ms, first call {} ms, teardown {} ms",
            ms(marks.spawn_ready()),
            ms(marks.first_call()),
            ms(marks.teardown())
        );
    }
    Ok(marks)
}

fn timings_json(m: &TimingMarks) -> Value {
    let ms = |d: Option<Duration>| d.map(|d| d.as_secs_f64() * 1000.0);
    json!({
        "spawn_ready_ms": ms(m.spawn_ready()),
        "first_call_ms": ms(m.first_call()),
        "teardown_ms": ms(m.teardown()),
    })
}

#[derive(Debug, Default)]
struct Outcome {
    exit_code: Option<i32>,
    signal: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
    /// Terminating signal received by `sandcrab` itself (Ctrl-C, SIGTERM, SIGHUP, SIGQUIT).
    interrupted: Option<i32>,
}

impl Outcome {
    fn code(&self) -> u8 {
        if let Some(sig) = self.interrupted {
            return util::exit_code_for(sig);
        }
        let code = match (self.exit_code, self.signal) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            (None, None) => 1,
        };
        u8::try_from(code.clamp(0, 255)).unwrap_or(1)
    }
}

/// Start the command as a background process and stream its output until it exits.
async fn stream(
    sb: &Sandbox,
    command: String,
    capture: bool,
    intr: &mut Interrupt,
) -> anyhow::Result<Outcome> {
    let started = sb
        .process_start_with(ProcessStartParams {
            command,
            cwd: None,
            env: Default::default(),
            pty: false,
        })
        .await
        .context("starting the command")?;
    let pid = started.process_id;
    let mut out = Outcome::default();
    let mut delay = POLL_MIN;
    loop {
        if intr.is_set() && out.interrupted.is_none() {
            out.interrupted = intr.signal();
            let _ = sb.process_kill(pid.as_str(), KillSignal::Kill).await;
        }
        let poll = sb
            .process_poll(pid.as_str())
            .await
            .context("polling the command")?;
        out.truncated |= poll.truncated;
        let had_output = !poll.stdout.is_empty() || !poll.stderr.is_empty();
        if capture {
            out.stdout.push_str(&poll.stdout);
            out.stderr.push_str(&poll.stderr);
        } else {
            if !poll.stdout.is_empty() {
                let mut o = std::io::stdout().lock();
                o.write_all(poll.stdout.as_bytes())?;
                o.flush()?;
            }
            if !poll.stderr.is_empty() {
                let mut e = std::io::stderr().lock();
                e.write_all(poll.stderr.as_bytes())?;
                e.flush()?;
            }
        }
        if !poll.running {
            out.exit_code = poll.exit_code;
            out.signal = poll.signal;
            return Ok(out);
        }
        delay = if had_output {
            POLL_MIN
        } else {
            (delay * 2).min(POLL_MAX)
        };
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = intr.wait(), if out.interrupted.is_none() => {}
        }
    }
}

pub async fn run(
    g: &Global,
    spawn_args: &SpawnArgs,
    keep: bool,
    command: &[String],
) -> anyhow::Result<u8> {
    let mut intr = Interrupt::install();
    let sc = util::sandcrab(g)?;
    let sb = spawn(g, &sc, spawn_args.to_config(keep)).await?;

    let outcome = if intr.is_set() {
        Ok(Outcome {
            interrupted: intr.signal(),
            ..Outcome::default()
        })
    } else {
        stream(&sb, shell_join(command), g.json, &mut intr).await
    };

    if keep && outcome.as_ref().is_ok_and(|o| o.interrupted.is_none()) {
        eprintln!(
            "sandcrab: keeping sandbox {} alive; press Ctrl-C to destroy it",
            sb.id()
        );
        intr.wait().await;
    }
    let id = sb.id();
    let marks = destroy(g, sb).await;
    let outcome = outcome?;
    let marks = marks?;
    if g.json {
        print_json(&json!({
            "sandbox_id": id,
            "exit_code": outcome.exit_code,
            "signal": outcome.signal,
            "interrupted": outcome.interrupted.is_some(),
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "truncated": outcome.truncated,
            "timings": timings_json(&marks),
        }))?;
    } else if outcome.truncated {
        eprintln!("sandcrab: warning: some output was dropped (buffer overflow between polls)");
    }
    Ok(outcome.code())
}

fn read_params(params: Option<&str>) -> anyhow::Result<Value> {
    let text = match params {
        None => return Ok(json!({})),
        Some("-") => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                .context("reading params from stdin")?;
            s
        }
        Some(s) => s.to_owned(),
    };
    serde_json::from_str(&text).context("params must be valid JSON")
}

pub async fn tool(
    g: &Global,
    spawn_args: &SpawnArgs,
    name: &str,
    params: Option<&str>,
) -> anyhow::Result<u8> {
    let input = read_params(params)?;
    let mut intr = Interrupt::install();
    let sc = util::sandcrab(g)?;
    let sb = spawn(g, &sc, spawn_args.to_config(false)).await?;
    let reply = tokio::select! {
        r = sb.tool_dispatch(name, input), if !intr.is_set() => Some(r),
        () = intr.wait() => None,
    };
    let id = sb.id();
    destroy(g, sb).await?;
    match reply {
        None => {
            eprintln!("sandcrab: interrupted; sandbox {id} destroyed");
            Ok(intr.exit_code())
        }
        Some(Ok(v)) => {
            print_json(&v)?;
            Ok(u8::from(is_tool_error(&v)))
        }
        Some(Err(e)) => {
            print_json(&e.to_json())?;
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        let o = |exit_code, signal| Outcome {
            exit_code,
            signal,
            ..Outcome::default()
        };
        assert_eq!(o(Some(0), None).code(), 0);
        assert_eq!(o(Some(3), None).code(), 3);
        assert_eq!(o(None, Some(9)).code(), 137);
        assert_eq!(o(None, None).code(), 1);
        assert_eq!(o(Some(300), None).code(), 255);
        let mut i = o(Some(0), None);
        i.interrupted = Some(2);
        assert_eq!(i.code(), 130);
        i.interrupted = Some(15);
        assert_eq!(i.code(), 143);
        i.interrupted = Some(1);
        assert_eq!(i.code(), 129);
    }

    #[test]
    fn params() {
        assert_eq!(read_params(None).unwrap(), json!({}));
        assert_eq!(
            read_params(Some(r#"{"pattern":"*"}"#)).unwrap()["pattern"],
            "*"
        );
        assert!(read_params(Some("{nope")).is_err());
    }
}
