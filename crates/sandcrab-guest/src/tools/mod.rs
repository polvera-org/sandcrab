//! Tool dispatch: routes a typed [`ToolCall`] to the module that implements it.
//!
//! Module ownership (one follow-up step each, so they never touch shared files):
//! - [`exec`] — `bash`
//! - [`process`] — `process_start` / `process_poll` / `process_stdin` / `process_kill` /
//!   `process_list`, plus the [`process::ProcessRegistry`] held by [`ToolContext`]
//! - [`fs`] — `file_read` / `file_write` / `file_edit` / `file_delete` / `file_move`
//! - [`transfer`] — `upload` / `download`
//! - [`search`] — `list_dir` / `glob` / `grep`
//!
//! Every tool fn has the shape
//! `pub async fn <tool>(ctx: &ToolContext, p: <Tool>Params) -> Result<<Tool>Result, ProtocolError>`.
//! Errors are returned as [`ProtocolError`] variants (`NotFound`, `ExecutionFailed`,
//! `TooLarge`, ...) and become JSON-RPC error objects in the server; messages are read by an
//! LLM, so say what went wrong and how to fix the call. Tool fns run on the multi-threaded
//! tokio runtime: wrap blocking filesystem / search work in `tokio::task::spawn_blocking`.

pub mod exec;
pub mod fs;
pub mod process;
pub mod search;
pub mod transfer;

use sandcrab_protocol::{ProtocolError, ToolCall, ToolResult};

use crate::ToolContext;

/// Run one tool call.
pub async fn dispatch(ctx: &ToolContext, call: ToolCall) -> Result<ToolResult, ProtocolError> {
    use ToolCall as C;
    Ok(match call {
        C::Bash(p) => exec::bash(ctx, p).await?.into(),
        C::ProcessStart(p) => process::process_start(ctx, p).await?.into(),
        C::ProcessPoll(p) => process::process_poll(ctx, p).await?.into(),
        C::ProcessStdin(p) => process::process_stdin(ctx, p).await?.into(),
        C::ProcessKill(p) => process::process_kill(ctx, p).await?.into(),
        C::ProcessList(p) => process::process_list(ctx, p).await?.into(),
        C::FileRead(p) => fs::file_read(ctx, p).await?.into(),
        C::FileWrite(p) => fs::file_write(ctx, p).await?.into(),
        C::FileEdit(p) => fs::file_edit(ctx, p).await?.into(),
        C::FileDelete(p) => fs::file_delete(ctx, p).await?.into(),
        C::FileMove(p) => fs::file_move(ctx, p).await?.into(),
        C::Upload(p) => transfer::upload(ctx, p).await?.into(),
        C::Download(p) => transfer::download(ctx, p).await?.into(),
        C::ListDir(p) => search::list_dir(ctx, p).await?.into(),
        C::Glob(p) => search::glob(ctx, p).await?.into(),
        C::Grep(p) => search::grep(ctx, p).await?.into(),
    })
}
