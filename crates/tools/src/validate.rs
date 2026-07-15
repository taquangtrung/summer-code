//! JSON Schema validation for tool arguments.
//!
//! **A real validator, not a subset.** A hand-rolled check that walks `required`
//! and the top level of `properties` catches the mistakes a well-behaved model
//! makes, which is the wrong bar: the schema is a contract the *untrusted* sides
//! of this system are held to. Phase 6 loads tools from external MCP servers,
//! and "external tools inherit schema validation" is only true if the validator
//! understands the schemas they ship. The subset silently accepted `items`
//! violations, read `integer` as `number` so `3.7` passed, and treated
//! `additionalProperties` and `enum` as decoration.
//!
//! Every failure here is handed back to the model as an `Outcome::Err`, so the
//! message is part of the interface: it is what the model reads to correct
//! itself, and it should name what was wrong rather than that something was.

use jsonschema::JSONSchema;
use serde_json::Value;

// ========================================================================
// Functions
// ========================================================================

/// Validate `args` against `schema`. Returns `Ok(())` or a human-readable
/// message handed back to the model as a tool error.
///
/// A schema that does not compile is a bug in the tool, not in the call, so it
/// rejects every call rather than passing them through unchecked. That is the
/// direction that fails safe: an MCP server shipping a broken schema gets a tool
/// nobody can call, not a tool nobody validates.
pub fn validate(args: &Value, schema: &Value) -> Result<(), String> {
    if !args.is_object() {
        return Err("arguments must be an object".into());
    }
    let compiled = JSONSchema::compile(schema)
        .map_err(|error| format!("the tool's input schema does not compile: {error}"))?;
    let Err(errors) = compiled.validate(args) else {
        return Ok(());
    };
    let mut messages: Vec<String> = errors
        .map(|error| format!("{} at `{}`", error, error.instance_path))
        .collect();
    messages.sort();
    Err(messages.join("; "))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::{Map, json};

    use super::*;
    use crate::{Builtin, ToolSource};

    fn schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    #[test]
    fn rejects_missing_required_field() {
        let args = serde_json::json!({ "path": "/x" });
        let error = validate(&args, &schema()).expect_err("content is required");
        assert!(
            error.contains("content"),
            "the model must be told what is missing: {error}"
        );
    }

    #[test]
    fn rejects_wrong_type() {
        let args = serde_json::json!({ "path": 42, "content": "x" });
        assert!(validate(&args, &schema()).is_err());
    }

    #[test]
    fn accepts_valid_arguments() {
        let args = serde_json::json!({ "path": "/x", "content": "x" });
        assert!(validate(&args, &schema()).is_ok());
    }

    #[test]
    fn rejects_a_bad_array_item() {
        // The subset never looked inside `items`. `git`'s schema declares
        // `args: [string]`, and `GitTool::call` filter_maps with `as_str`, so a
        // non-string was silently *dropped*: `git commit -m 42` ran as
        // `git commit -m`, and the model was never told its call was mangled.
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "args": { "type": "array", "items": { "type": "string" } } }
        });
        let args = serde_json::json!({ "args": ["commit", "-m", 42] });
        assert!(
            validate(&args, &schema).is_err(),
            "a non-string in a string array must not be silently dropped"
        );
    }

    #[test]
    fn integer_does_not_accept_a_float() {
        // The subset mapped `integer` and `number` to the same check. No
        // built-in uses `integer`; every MCP server's schema will.
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "count": { "type": "integer" } }
        });
        assert!(validate(&serde_json::json!({ "count": 3.7 }), &schema).is_err());
        assert!(validate(&serde_json::json!({ "count": 3 }), &schema).is_ok());
    }

    #[test]
    fn honors_additional_properties_and_enum() {
        // Neither was checked at all. An MCP server relying on either to bound
        // what it accepts was unprotected.
        let strict = serde_json::json!({
            "type": "object",
            "properties": { "mode": { "enum": ["read", "write"] } },
            "additionalProperties": false
        });
        assert!(validate(&serde_json::json!({ "mode": "read" }), &strict).is_ok());
        assert!(validate(&serde_json::json!({ "mode": "exec" }), &strict).is_err());
        assert!(
            validate(&serde_json::json!({ "mode": "read", "extra": 1 }), &strict).is_err(),
            "additionalProperties: false must reject an unknown field"
        );
    }

    #[test]
    fn honors_nested_object_properties() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "opts": {
                    "type": "object",
                    "properties": { "depth": { "type": "integer" } }
                }
            }
        });
        assert!(validate(&serde_json::json!({ "opts": { "depth": "deep" } }), &schema).is_err());
    }

    #[test]
    fn a_schema_that_does_not_compile_rejects_rather_than_waves_through() {
        // The safe direction: a tool with a broken schema is uncallable, not
        // unvalidated.
        let broken = serde_json::json!({ "type": "not-a-type" });
        assert!(validate(&serde_json::json!({}), &broken).is_err());
    }

    // ====================================================================
    // Properties
    // ====================================================================

    /// A JSON value, shallow enough to keep the recursion honest.
    ///
    /// Object keys are drawn from the schema keywords as well as from arbitrary
    /// words, so a generated value is sometimes a *meaningful* schema (`{"type":
    /// "integer"}`) rather than always noise. Noise alone would exercise only
    /// the compile-error path, which is the one path this property does not need
    /// help reaching.
    fn value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            (-1000.0_f64..1000.0).prop_map(Value::from),
            keyword_or_word().prop_map(Value::from),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                proptest::collection::vec((keyword_or_word(), inner), 0..4)
                    .prop_map(|pairs| Value::Object(pairs.into_iter().collect())),
            ]
        })
    }

    /// A schema keyword, a type name, or an ordinary word.
    fn keyword_or_word() -> impl Strategy<Value = String> {
        prop_oneof![
            6 => prop::sample::select(vec![
                "type",
                "properties",
                "required",
                "items",
                "enum",
                "additionalProperties",
                "$ref",
            ]).prop_map(String::from),
            3 => prop::sample::select(vec![
                "string", "integer", "number", "boolean", "object", "array", "null",
            ]).prop_map(String::from),
            2 => "[a-z]{1,6}".prop_map(String::from),
        ]
    }

    /// A value satisfying `schema`'s declared type, built from generated parts.
    ///
    /// Returns `None` for a type this helper does not know, which is a signal
    /// rather than a skip: a tool schema that starts declaring one fails the
    /// property below until someone teaches this function what it means.
    fn sample(schema: &Value, text: &str, number: i64, flag: bool) -> Option<Value> {
        if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
            return choices.first().cloned();
        }
        match schema.get("type").and_then(Value::as_str)? {
            "array" => {
                let items = schema.get("items").cloned().unwrap_or(json!({}));
                Some(json!([sample(&items, text, number, flag)?]))
            }
            "boolean" => Some(Value::from(flag)),
            // Lifted to the schema's `minimum`: a value below it is a call the
            // schema refuses, so it cannot stand in for a filled one.
            "integer" | "number" => {
                let floor = schema.get("minimum").and_then(Value::as_i64);
                Some(Value::from(floor.map_or(number, |floor| number.max(floor))))
            }
            "object" => Some(Value::Object(Map::new())),
            "string" => Some(Value::from(text)),
            _ => None,
        }
    }

    /// Every declared property of `schema`, filled with a value of its type.
    fn filled(schema: &Value, text: &str, number: i64, flag: bool) -> Map<String, Value> {
        let mut args = Map::new();
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (name, property) in properties {
            let filled = sample(&property, text, number, flag)
                .unwrap_or_else(|| panic!("`{name}` declares a type `sample` does not know"));
            args.insert(name, filled);
        }
        args
    }

    fn required_of(schema: &Value) -> Vec<String> {
        schema
            .get("required")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    proptest! {
        /// Validation answers, whatever the model sent and whatever schema the
        /// tool shipped.
        ///
        /// Both sides are untrusted: arguments come from the model, and a schema
        /// comes from an MCP server this build has never seen. Today every
        /// failure path returns a message, and the bug this guards against is a
        /// future `unwrap` on the compile `Result` or on an error's instance
        /// path, which would turn a malformed schema into a panic inside the
        /// dispatcher with a tool call in flight.
        #[test]
        fn validation_answers_for_any_arguments_and_any_schema(
            args in value(),
            schema in value(),
        ) {
            // The assertion is that this returns at all; proptest shrinks
            // whatever input panics.
            let _ = validate(&args, &schema);
        }

        /// For every built-in tool: a call with every declared property filled
        /// validates, dropping any *required* property is refused by name, and
        /// dropping an optional one is still fine.
        ///
        /// Three failures in one property, and each has shipped somewhere:
        /// arguments the model built correctly being refused, a missing field
        /// reaching the tool as `None`, and an "optional" field that was not.
        /// The tools come from the registry rather than a list here, so a tool
        /// added tomorrow is covered without anyone remembering to add it.
        #[test]
        fn every_builtin_schema_accepts_a_filled_call_and_names_what_is_missing(
            text in "[a-z][a-z0-9_/.-]{0,12}",
            number in 0_i64..1_000,
            flag in any::<bool>(),
        ) {
            for tool in Builtin::new().tools() {
                let name = tool.spec().name.clone();
                let schema = &tool.spec().input_schema;
                let args = filled(schema, &text, number, flag);

                prop_assert!(
                    validate(&Value::Object(args.clone()), schema).is_ok(),
                    "{name} refused a call with every property filled: {:?}",
                    validate(&Value::Object(args.clone()), schema),
                );

                let required = required_of(schema);
                for field in &required {
                    let mut short = args.clone();
                    prop_assert!(
                        short.remove(field).is_some(),
                        "{name} requires `{field}`, which is not in its properties",
                    );
                    let error = validate(&Value::Object(short), schema)
                        .expect_err(&format!("{name} accepted a call missing `{field}`"));
                    prop_assert!(
                        error.contains(field),
                        "{name} refused a call without telling the model `{field}` was missing: {error}",
                    );
                }

                for field in args.keys().filter(|key| !required.contains(key)) {
                    let mut without = args.clone();
                    without.remove(field);
                    prop_assert!(
                        validate(&Value::Object(without), schema).is_ok(),
                        "{name} treated the optional `{field}` as required",
                    );
                }
            }
        }
    }
}
