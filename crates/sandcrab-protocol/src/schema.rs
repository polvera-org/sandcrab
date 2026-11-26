//! JSON Schema export, post-processed for LLM tool-calling providers.
//!
//! schemars 0.8 output is cleaned so every input schema is a self-contained object:
//! subschemas are inlined (no `$ref`/`definitions`), `Option<T>` is plain `T` (absence,
//! not `null`, means "use the default"), and the root `$schema`/`title`/`description` plus
//! non-standard integer `format`s (`uint64`, ...) are removed; empty roots get `properties: {}`.

use schemars::{JsonSchema, r#gen::SchemaSettings};
use serde::Serialize;
use serde_json::Value;

/// LLM / MCP tool definition: name, description and JSON Schemas.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolDefinition {
    /// Tool (JSON-RPC method) name.
    pub name: &'static str,
    /// LLM-facing description of what the tool does and when to use it.
    pub description: &'static str,
    /// JSON Schema of the params object (root `type: object`).
    pub input_schema: Value,
    /// JSON Schema of the result object.
    pub output_schema: Value,
}

/// JSON Schemas of a `crabd` method that is not a tool: the methods on a tool connection,
/// the connection-level verbs (except `auth`, whose token type has no schema), their
/// notifications and the `EXIT` record payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MethodSchema {
    /// Method name (or `record:EXIT` for the record payload).
    pub method: &'static str,
    /// How it is used: `call`, `connection`, `notification` or `record`.
    pub kind: &'static str,
    /// JSON Schema of the params object (or the payload).
    pub params: Value,
    /// JSON Schema of the result; `None` for notifications and records.
    pub result: Option<Value>,
}

fn method<P: JsonSchema, R: JsonSchema>(method: &'static str, kind: &'static str) -> MethodSchema {
    MethodSchema {
        method,
        kind,
        params: input_schema::<P>(),
        result: Some(output_schema::<R>()),
    }
}

fn one_way<P: JsonSchema>(method: &'static str, kind: &'static str) -> MethodSchema {
    MethodSchema {
        method,
        kind,
        params: output_schema::<P>(),
        result: None,
    }
}

/// Schemas of every non-tool method of this protocol version, in a stable order.
#[must_use]
pub fn method_schemas() -> Vec<MethodSchema> {
    use crate::*;
    vec![
        method::<HelloParams, HelloResult>(METHOD_HELLO, "call"),
        method::<PingParams, PingResult>(METHOD_PING, "call"),
        method::<ProcessCancelParams, ProcessCancelResult>(METHOD_PROCESS_CANCEL, "call"),
        method::<FsIdentityParams, FsIdentityResult>(METHOD_FS_IDENTITY, "call"),
        method::<ForwardStatsParams, ForwardStatsResult>(METHOD_FORWARD_STATS, "call"),
        method::<TunnelOpenParams, TunnelOpenAck>(METHOD_TUNNEL_OPEN, "connection"),
        method::<ProcessOpenParams, ProcessOpenAck>(METHOD_PROCESS_OPEN, "connection"),
        method::<ForwardListenParams, ForwardListenAck>(METHOD_FORWARD_LISTEN, "connection"),
        method::<ForwardAcceptParams, ForwardAcceptAck>(METHOD_FORWARD_ACCEPT, "connection"),
        one_way::<ForwardIncoming>(METHOD_FORWARD_INCOMING, "notification"),
        one_way::<ForwardRejectParams>(METHOD_FORWARD_REJECT, "notification"),
        one_way::<ProcessExit>("record:EXIT", "record"),
    ]
}

/// Clean input schema: `Option<T>` rendered as optional `T`.
#[must_use]
pub fn input_schema<T: JsonSchema>() -> Value {
    schema_for::<T>(false)
}

/// Clean output schema: `Option<T>` rendered as nullable `T`.
#[must_use]
pub fn output_schema<T: JsonSchema>() -> Value {
    schema_for::<T>(true)
}

fn schema_for<T: JsonSchema>(nullable_options: bool) -> Value {
    let settings = SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.option_add_null_type = nullable_options;
        s.option_nullable = false;
    });
    let root = settings.into_generator().into_root_schema_for::<T>();
    let mut value = serde_json::to_value(root).expect("schemas always serialize to JSON");
    if let Value::Object(map) = &mut value {
        for key in ["$schema", "title", "description", "definitions"] {
            map.remove(key);
        }
        // Some providers require `properties` on every object root, even when empty.
        if map.get("type").and_then(Value::as_str) == Some("object") {
            map.entry("properties")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
        }
    }
    clean(&mut value);
    value
}

/// Recursively clean a schema node, walking only schema-valued keywords so property
/// names that collide with keywords are never touched.
fn clean(node: &mut Value) {
    let Value::Object(map) = node else { return };

    if map.get("default") == Some(&Value::Null) {
        map.remove("default");
    }
    let nonstandard_format = map.get("format").and_then(Value::as_str).is_some_and(|f| {
        f.starts_with("int") || f.starts_with("uint") || f == "float" || f == "double"
    });
    if nonstandard_format {
        map.remove("format");
    }
    // schemars emits `minimum: 0.0` for unsigned ints; keep integer bounds integral.
    for key in ["minimum", "maximum"] {
        if let Some(Value::Number(n)) = map.get(key)
            && let Some(f) = n.as_f64()
            && f.fract() == 0.0
            && f.abs() < 9.0e15
        {
            map.insert(key.to_owned(), Value::from(f as i64));
        }
    }

    if let Some(Value::Object(props)) = map.get_mut("properties") {
        props.values_mut().for_each(clean);
    }
    for key in ["items", "additionalProperties", "not"] {
        if let Some(child) = map.get_mut(key) {
            clean(child);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(children)) = map.get_mut(key) {
            children.iter_mut().for_each(clean);
        }
    }
}
