//! Schema snapshot + LLM-compatibility checks.
//!
//! Regenerate the snapshots (`tool_definitions.json`, `method_schemas.json`,
//! `server-api.schema.json`) with:
//! `UPDATE_SNAPSHOTS=1 cargo test -p sandcrab-protocol --test schema`

use std::path::PathBuf;

use sandcrab_protocol::*;
use serde_json::Value;

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name)
}

#[test]
fn tool_definitions_match_snapshot() {
    check_snapshot(
        "tool_definitions.json",
        &serde_json::to_string_pretty(&tool_definitions()).unwrap(),
    );
}

#[test]
fn method_schemas_match_snapshot() {
    check_snapshot(
        "method_schemas.json",
        &serde_json::to_string_pretty(&method_schemas()).unwrap(),
    );
}

#[test]
fn method_schemas_cover_the_non_tool_methods() {
    let names: Vec<_> = method_schemas().iter().map(|m| m.method).collect();
    for m in [
        METHOD_HELLO,
        METHOD_PING,
        METHOD_PROCESS_CANCEL,
        METHOD_FS_IDENTITY,
        METHOD_FORWARD_STATS,
        METHOD_TUNNEL_OPEN,
        METHOD_PROCESS_OPEN,
        METHOD_FORWARD_LISTEN,
        METHOD_FORWARD_ACCEPT,
        METHOD_FORWARD_INCOMING,
        METHOD_FORWARD_REJECT,
    ] {
        assert!(names.contains(&m), "{m}");
        assert!(!TOOL_NAMES.contains(&m), "{m} must not be a tool");
    }
    for m in method_schemas() {
        walk(&m.params, &mut |o| {
            assert!(!o.contains_key("$ref"), "{}: $ref", m.method);
        });
        // Params of requests reject unknown fields.
        if m.result.is_some() && m.params["type"] == "object" {
            assert_eq!(m.params["additionalProperties"], false, "{}", m.method);
        }
    }
}

/// The one schema document of the server API; the TypeScript SDK copies it and generates its
/// types from it.
#[test]
fn server_api_schema_matches_snapshot() {
    check_snapshot(
        "server-api.schema.json",
        &serde_json::to_string_pretty(&api::server_api_schema()).unwrap(),
    );
}

#[test]
fn server_api_schema_is_complete_and_resolvable() {
    let doc = api::server_api_schema();
    let defs = doc["$defs"].as_object().unwrap();
    let def_of = |r: &Value| -> Value {
        match r["$ref"].as_str() {
            Some(path) => {
                let name = path.strip_prefix("#/$defs/").expect("local $defs ref");
                defs.get(name)
                    .unwrap_or_else(|| panic!("{name} missing"))
                    .clone()
            }
            None => r.clone(),
        }
    };
    // Every $ref anywhere resolves.
    walk(&doc, &mut |m| {
        if let Some(r) = m.get("$ref") {
            def_of(&serde_json::json!({ "$ref": r }));
        }
    });

    let methods = doc["methods"].as_object().unwrap();
    for (name, kind) in [
        (api::METHOD_SERVER_HELLO, "control"),
        (api::METHOD_SERVER_CAPABILITIES, "control"),
        (api::METHOD_SERVER_SELFTEST, "control"),
        (api::METHOD_SANDBOX_CREATE, "control"),
        (api::METHOD_SANDBOX_GET, "control"),
        (api::METHOD_SANDBOX_LIST, "control"),
        (api::METHOD_SANDBOX_SET_TTL, "control"),
        (api::METHOD_SANDBOX_DESTROY, "control"),
        (api::METHOD_SANDBOX_DESTROY_BY_LABEL, "control"),
        (api::METHOD_PROCESS_CANCEL, "control"),
        (api::METHOD_FORWARD_SERVE, "control"),
        (api::METHOD_FORWARD_REJECT, "control"),
        (api::METHOD_PROCESS_SPAWN, "connection"),
        (api::METHOD_FORWARD_ATTACH, "connection"),
    ] {
        let m = &methods[name];
        assert_eq!(m["kind"], kind, "{name}");
        let params = def_of(&m["params"]);
        assert_eq!(params["type"], "object", "{name}");
        assert_eq!(params["additionalProperties"], false, "{name} params");
        let result = def_of(&m["result"]);
        assert_ne!(result["additionalProperties"], false, "{name} result");
    }
    assert_eq!(
        methods.len(),
        14,
        "only the methods of design §5.3 and §5.4"
    );
    for cut in ["file", "tool", "template", "pty", "signal", "workspace"] {
        assert!(
            !methods.keys().any(|k| k.contains(cut)),
            "{cut}: not in this API"
        );
    }
    // The spawn params are `sandbox_id` plus every field of `process.open`.
    let spawn = def_of(&methods[api::METHOD_PROCESS_SPAWN]["params"]);
    let open = serde_json::to_value(ProcessOpenParams::new("x", vec![])).unwrap();
    for field in open
        .as_object()
        .unwrap()
        .keys()
        .chain(["sandbox_id".to_owned()].iter())
    {
        assert!(spawn["properties"].get(field).is_some(), "{field}");
    }

    for n in [
        api::NOTIFY_FORWARD_INCOMING,
        api::NOTIFY_SANDBOX_DESTROYED,
        api::NOTIFY_SERVER_SHUTDOWN,
    ] {
        assert_eq!(
            def_of(&doc["notifications"][n]["params"])["type"],
            "object",
            "{n}"
        );
    }
    assert_eq!(doc["errors"].as_object().unwrap().len(), 10);
    assert_eq!(
        doc["errors"]["UNAVAILABLE"]["code"],
        api::codes::UNAVAILABLE
    );
    assert!(defs.contains_key("ProcessExit"));
    // `ttl_secs` of `sandbox.set_ttl` is required and nullable; of `sandbox.create`, optional
    // and nullable (absent: the default; null: no TTL).
    let set_ttl = &defs["SandboxSetTtlParams"];
    assert_eq!(
        set_ttl["properties"]["ttl_secs"]["type"],
        serde_json::json!(["integer", "null"])
    );
    assert!(
        set_ttl["required"]
            .as_array()
            .unwrap()
            .contains(&"ttl_secs".into())
    );
    let create = &defs["SandboxCreate"];
    assert_eq!(
        create["properties"]["ttl_secs"]["type"],
        serde_json::json!(["integer", "null"])
    );
    assert_eq!(create["required"], serde_json::json!(["template"]));
    assert_eq!(doc["api_version"], api::API_VERSION);
}

fn check_snapshot(name: &str, actual: &str) {
    let actual = actual.to_owned() + "\n";
    let path = snapshot_path(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot {} ({e}); run with UPDATE_SNAPSHOTS=1",
            path.display()
        )
    });
    assert!(
        actual == expected,
        "{name} changed; review and re-run with UPDATE_SNAPSHOTS=1"
    );
}

fn walk(v: &Value, f: &mut impl FnMut(&serde_json::Map<String, Value>)) {
    match v {
        Value::Object(m) => {
            f(m);
            m.values().for_each(|c| walk(c, f));
        }
        Value::Array(a) => a.iter().for_each(|c| walk(c, f)),
        _ => {}
    }
}

#[test]
fn input_schemas_are_llm_friendly() {
    for def in tool_definitions() {
        let s = &def.input_schema;
        assert_eq!(s["type"], "object", "{} root must be an object", def.name);
        assert_eq!(
            s["additionalProperties"], false,
            "{} must reject unknown fields",
            def.name
        );
        assert!(s.get("$schema").is_none() && s.get("title").is_none());
        assert!(
            def.description.len() > 40,
            "{} needs a real description",
            def.name
        );

        walk(s, &mut |m| {
            assert!(!m.contains_key("$ref"), "{}: $ref in schema", def.name);
            assert!(!m.contains_key("definitions"), "{}: definitions", def.name);
            assert!(
                !matches!(m.get("type"), Some(Value::Array(_))),
                "{}: type arrays are not portable",
                def.name
            );
            if let Some(f) = m.get("format").and_then(Value::as_str) {
                assert!(!f.contains("int"), "{}: non-standard format {f}", def.name);
            }
        });

        // Every property carries an LLM-facing description.
        let props = s["properties"].as_object().expect("properties present");
        for (name, prop) in props {
            assert!(
                prop["description"].as_str().is_some_and(|d| !d.is_empty()),
                "{}.{name} lacks a description",
                def.name
            );
        }
    }
}

#[test]
fn output_schemas_are_objects_without_refs() {
    for def in tool_definitions() {
        assert_eq!(def.output_schema["type"], "object", "{}", def.name);
        walk(&def.output_schema, &mut |m| {
            assert!(!m.contains_key("$ref"), "{}: $ref in output", def.name);
        });
    }
}
