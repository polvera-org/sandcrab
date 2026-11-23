//! `bash`: run a shell command to completion.
//!
//! Contract (see `BashParams` / `BashResult` in `sandcrab-protocol` and `docs/ROADMAP.md`):
//! - Run `bash -c <command>` with cwd = `ctx.resolve_path_or_default(p.cwd)`; the daemon env
//!   plus `p.env`. Missing cwd → `ProtocolError::NotFound`. Spawn failure →
//!   `ProtocolError::ExecutionFailed`. stdin is `/dev/null`.
//! - The child leads its **own process group** (`process_group(0)`); on timeout send
//!   SIGKILL to the whole group (`killpg`) so grandchildren die too, then reap. Timeout =
//!   `p.timeout_ms` or `ctx.limits.bash_default_timeout_ms`, capped at
//!   `ctx.limits.bash_max_timeout_ms`. A timeout is *not* an RPC error: return the result
//!   with `timed_out = true`, `exit_code = None`, `signal = Some(9)` and the output so far.
//! - Capture stdout and stderr concurrently. Keep at most `ctx.limits.bash_max_output_bytes`
//!   per stream as head + tail with a `[... N bytes truncated ...]` marker in the middle
//!   (bounded memory while reading, never buffer unbounded output); set `truncated`.
//!   Decode lossily as UTF-8.
//! - `exit_code` / `signal` from the exit status; `duration_ms` = wall clock.
//! - If the request future is dropped (connection closed / daemon shutdown), the process
//!   group must still be killed (e.g. `kill_on_drop` + a group-kill guard).
//! - Syscalls only via safe APIs (`unsafe_code = "forbid"`): std / tokio `process_group`,
//!   `rustix` for `killpg`.

use sandcrab_protocol::{BashParams, BashResult, ProtocolError};

use crate::ToolContext;

/// Run `p.command` with `bash -c`. See the module docs for the contract.
pub async fn bash(ctx: &ToolContext, p: BashParams) -> Result<BashResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("bash"))
}
