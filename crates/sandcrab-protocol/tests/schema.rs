//! Schema snapshot + LLM-compatibility checks.
//!
//! Regenerate the snapshot with:
//! `UPDATE_SNAPSHOTS=1 cargo test -p sandcrab-protocol --test schema`

use std::path::PathBuf;

use sandcrab_protocol::*;
use serde_json::Value;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/tool_definitions.json")
}

#[test]
fn tool_definitions_match_snapshot() {
    let actual = serde_json::to_string_pretty(&tool_definitions()).unwrap() + "\n";
    let path = snapshot_path();
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
        "tool definitions changed; review and re-run with UPDATE_SNAPSHOTS=1"
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
