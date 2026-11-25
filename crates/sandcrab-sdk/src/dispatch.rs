//! LLM tool-use bridge: `(tool name, JSON input)` → protocol call → JSON.
//!
//! Used by the `crabagent` harness and future adapters (pi-coding-agent, MCP). Tool names
//! are exactly the names in [`sandcrab_protocol::tool_definitions`].

use sandcrab_core::SandboxError;
use sandcrab_protocol::{ProtocolError, TOOL_NAMES, ToolCall};
use serde::Serialize;
use serde_json::{Value, json};

use crate::Sandbox;

/// A failed tool invocation, in a shape an LLM can read.
///
/// `fatal = false`: the model made a recoverable mistake or the tool failed (unknown tool,
/// bad params, file not found, ambiguous edit, timeout, ...) — feed [`Self::to_json`] back
/// as an error tool result. `fatal = true`: the sandbox itself is unusable (destroyed,
/// transport lost) — the harness should stop or respawn rather than retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ToolError {
    /// Stable snake_case code, e.g. `unknown_tool`, `invalid_params`, `not_found`.
    pub code: String,
    pub message: String,
    #[serde(skip)]
    pub fatal: bool,
}

impl ToolError {
    fn new(code: &str, message: impl Into<String>, fatal: bool) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            fatal,
        }
    }

    /// `{"error": {"code": "...", "message": "..."}}`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "error": { "code": self.code, "message": self.message } })
    }

    fn from_protocol(err: &ProtocolError) -> Self {
        let code = match err {
            ProtocolError::Parse(_) => "parse_error",
            ProtocolError::InvalidRequest(_) => "invalid_request",
            ProtocolError::MethodNotFound(_) => "unknown_tool",
            ProtocolError::InvalidParams(_) => "invalid_params",
            ProtocolError::Internal(_) => "internal_error",
            ProtocolError::ExecutionFailed(_) => "execution_failed",
            ProtocolError::TimedOut { .. } => "timed_out",
            ProtocolError::PathNotAllowed(_) => "path_not_allowed",
            ProtocolError::NotFound(_) => "not_found",
            ProtocolError::AmbiguousEdit { .. } => "ambiguous_edit",
            ProtocolError::TooLarge { .. } => "too_large",
            ProtocolError::Unimplemented(_) => "unimplemented",
            // crabd refused the connection itself: nothing the model can retry.
            ProtocolError::Unauthorized(_) => {
                return Self::new("unauthorized", err.to_string(), true);
            }
            ProtocolError::Other { .. } => "tool_error",
        };
        Self::new(code, err.to_string(), false)
    }
}

impl From<SandboxError> for ToolError {
    fn from(err: SandboxError) -> Self {
        match &err {
            SandboxError::Protocol(p) => Self::from_protocol(p),
            // The call took too long on the host side; the sandbox may still be fine.
            SandboxError::Timeout { .. } => Self::new("timed_out", err.to_string(), false),
            SandboxError::NotFound {
                kind: "sandbox", ..
            } => Self::new(
                "sandbox_unavailable",
                "the sandbox has been destroyed",
                true,
            ),
            SandboxError::Transport(_) => Self::new("transport_error", err.to_string(), true),
            _ => Self::new("sandbox_error", err.to_string(), true),
        }
    }
}

/// Whether a [`Sandbox::tool_dispatch`] reply is a rendered (non-fatal) tool error.
#[must_use]
pub fn is_tool_error(reply: &Value) -> bool {
    reply.get("error").is_some_and(|e| e.get("code").is_some())
        && reply.as_object().is_some_and(|o| o.len() == 1)
}

/// Parse an LLM tool-use into a typed call. `null` input means `{}`.
pub(crate) fn parse_call(name: &str, input: Value) -> Result<ToolCall, ToolError> {
    if !TOOL_NAMES.contains(&name) {
        return Err(ToolError::new(
            "unknown_tool",
            format!(
                "unknown tool `{name}`; available tools: {}",
                TOOL_NAMES.join(", ")
            ),
            false,
        ));
    }
    ToolCall::from_method_params(name, input).map_err(|e| {
        let mut err = ToolError::from_protocol(&e);
        if let ProtocolError::InvalidParams(detail) = &e {
            err.message = format!("invalid input for `{name}`: {detail}");
        }
        err
    })
}

impl Sandbox {
    /// Run an LLM tool-use and return the tool's JSON result, or every failure as a
    /// [`ToolError`] (check [`ToolError::fatal`]).
    pub async fn tool_call(&self, name: &str, input: Value) -> Result<Value, ToolError> {
        let call = parse_call(name, input)?;
        let result = self.call(call).await?;
        Ok(result.to_value())
    }

    /// Run an LLM tool-use for an agent loop.
    ///
    /// - success → the tool's result JSON (e.g. `{"exit_code":0,"stdout":..}` for `bash`);
    /// - recoverable failure (unknown tool, invalid input, tool error) → `Ok` with
    ///   `{"error": {"code", "message"}}` for the model to read (see [`is_tool_error`] to
    ///   set an `is_error` flag);
    /// - fatal failure (sandbox gone, transport lost) → `Err`.
    pub async fn tool_dispatch(&self, name: &str, input: Value) -> Result<Value, ToolError> {
        match self.tool_call(name, input).await {
            Ok(v) => Ok(v),
            Err(e) if !e.fatal => Ok(e.to_json()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use sandcrab_core::testing::FakeBackend;
    use sandcrab_protocol::tool_definitions;

    use super::*;
    use crate::SandboxConfig;
    use crate::test_support::Env;

    #[test]
    fn every_defined_tool_is_dispatchable() {
        for def in tool_definitions() {
            match parse_call(def.name, Value::Null) {
                Ok(call) => assert_eq!(call.name(), def.name),
                Err(e) => assert_eq!(e.code, "invalid_params", "{}: {e}", def.name),
            }
        }
    }

    #[test]
    fn parse_errors_are_structured() {
        let e = parse_call("rm_rf", json!({})).unwrap_err();
        assert_eq!(e.code, "unknown_tool");
        assert!(!e.fatal);
        assert!(e.message.contains("file_read"), "{}", e.message);

        let e = parse_call("bash", json!({})).unwrap_err();
        assert_eq!(e.code, "invalid_params");
        assert!(e.message.contains("command"), "{}", e.message);

        let e = parse_call("bash", json!({"command": "ls", "bogus": 1})).unwrap_err();
        assert_eq!(e.code, "invalid_params");

        let e = parse_call("glob", json!("**/*.rs")).unwrap_err();
        assert_eq!(e.code, "invalid_params");

        let call = parse_call("process_list", Value::Null).unwrap();
        assert_eq!(call.name(), "process_list");
    }

    #[tokio::test]
    async fn dispatch_maps_results_and_errors() {
        let env = Env::new();
        let backend = FakeBackend::new();
        let sc = env.sandcrab(&backend);
        let sb = sc
            .spawn(SandboxConfig::from_template("base"))
            .await
            .unwrap();

        let ok = sb
            .tool_dispatch("bash", json!({"command": "echo hi"}))
            .await
            .unwrap();
        assert_eq!(ok["stdout"], "echo hi");
        assert_eq!(ok["exit_code"], 0);
        assert!(!is_tool_error(&ok));

        let unknown = sb.tool_dispatch("nope", json!({})).await.unwrap();
        assert!(is_tool_error(&unknown));
        assert_eq!(unknown["error"]["code"], "unknown_tool");

        let bad = sb
            .tool_dispatch("bash", json!({"cmd": "ls"}))
            .await
            .unwrap();
        assert_eq!(bad["error"]["code"], "invalid_params");

        // mock crabd answers file_read with NotFound
        let nf = sb
            .tool_dispatch("file_read", json!({"path": "missing.txt"}))
            .await
            .unwrap();
        assert_eq!(nf["error"]["code"], "not_found");
        assert!(
            nf["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing.txt")
        );

        // mock crabd: other tools are unimplemented
        let un = sb
            .tool_dispatch("glob", json!({"pattern": "*"}))
            .await
            .unwrap();
        assert_eq!(un["error"]["code"], "unimplemented");

        let core = sb.core().clone();
        sb.destroy().await.unwrap();
        let fatal = crate::Sandbox::new(core)
            .tool_dispatch("bash", json!({"command": "ls"}))
            .await
            .unwrap_err();
        assert!(fatal.fatal);
        assert_eq!(fatal.code, "sandbox_unavailable");
        assert_eq!(fatal.to_json()["error"]["code"], "sandbox_unavailable");
    }
}
