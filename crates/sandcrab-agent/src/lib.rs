//! `crabagent`: a minimal, pi-coding-agent-style coding agent that drives Claude (Anthropic
//! Messages API) whose tools are exactly Sandcrab's sandbox tools.
//!
//! - [`llm`]: the LLM client trait, the streaming Anthropic client and a scripted fake.
//! - [`agent`]: the tool-use loop (parallel tool calls via `Sandbox::tool_dispatch`).
//! - [`session`]: workspace resolution, sandbox lifecycle, REPL / one-shot.

pub mod agent;
pub mod cli;
pub mod cost;
pub mod llm;
pub mod prompt;
pub mod session;
pub mod tools;
pub mod transcript;
pub mod ui;
