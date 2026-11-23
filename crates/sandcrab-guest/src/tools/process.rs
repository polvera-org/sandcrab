//! Background processes: `process_start` / `process_poll` / `process_stdin` /
//! `process_kill` / `process_list`, and the [`ProcessRegistry`] that tracks them.
//!
//! Contract (see the `Process*Params` / `Process*Result` types in `sandcrab-protocol`):
//! - `process_start`: spawn `bash -c <command>` in its own process group with cwd =
//!   `ctx.resolve_path_or_default(p.cwd)` and env + `p.env`; return immediately with a fresh
//!   `process_id` (opaque string, e.g. `p-<n>`) and the OS pid. `pty = true` attaches a
//!   pseudo-terminal (stdout carries the PTY output, stderr stays empty). More than
//!   `ctx.limits.process_max_count` tracked processes → `ExecutionFailed` (tell the caller
//!   to kill some).
//! - Output is drained continuously by background tasks into per-stream buffers bounded by
//!   `ctx.limits.process_max_buffer_bytes`; on overflow drop the oldest bytes and flag
//!   `truncated` for the next poll.
//! - `process_poll`: return and clear the output buffered since the previous poll, plus
//!   `running` / `exit_code` / `signal`. Unknown id → `NotFound`.
//! - `process_stdin`: write `data` to stdin (PTY master when `pty`); exited process or closed
//!   stdin → `ExecutionFailed`.
//! - `process_kill`: send `p.signal` to the whole process group (`killpg`);
//!   `was_running = false` if it had already exited. Not an error for exited processes.
//! - `process_list`: every tracked process, oldest first.
//! - [`ProcessRegistry::shutdown`] is called once by the server on graceful shutdown: kill
//!   every live process group (TERM, short grace, then KILL) and reap.
//! - Syscalls only via safe APIs (`unsafe_code = "forbid"`): tokio `process`, `rustix`.

use sandcrab_protocol::{
    ProcessKillParams, ProcessKillResult, ProcessListParams, ProcessListResult, ProcessPollParams,
    ProcessPollResult, ProcessStartParams, ProcessStartResult, ProcessStdinParams,
    ProcessStdinResult, ProtocolError,
};

use crate::ToolContext;

/// Registry of background processes, held by [`ToolContext::processes`].
///
/// Placeholder: the process step replaces the internals. It must stay `Default + Send + Sync`
/// (use interior mutability) and keep [`ProcessRegistry::shutdown`].
#[derive(Debug, Default)]
pub struct ProcessRegistry {
    _private: (),
}

impl ProcessRegistry {
    /// Kill and reap every live process. Called by the server during graceful shutdown.
    pub async fn shutdown(&self) {}
}

/// Start a background process.
pub async fn process_start(
    ctx: &ToolContext,
    p: ProcessStartParams,
) -> Result<ProcessStartResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("process_start"))
}

/// Status and new output of a background process.
pub async fn process_poll(
    ctx: &ToolContext,
    p: ProcessPollParams,
) -> Result<ProcessPollResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("process_poll"))
}

/// Write to a background process's stdin.
pub async fn process_stdin(
    ctx: &ToolContext,
    p: ProcessStdinParams,
) -> Result<ProcessStdinResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("process_stdin"))
}

/// Signal a background process's group.
pub async fn process_kill(
    ctx: &ToolContext,
    p: ProcessKillParams,
) -> Result<ProcessKillResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("process_kill"))
}

/// List tracked background processes.
pub async fn process_list(
    ctx: &ToolContext,
    p: ProcessListParams,
) -> Result<ProcessListResult, ProtocolError> {
    let _ = (ctx, p);
    Err(super::unimplemented("process_list"))
}
