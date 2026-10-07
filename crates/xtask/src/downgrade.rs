//! Lowers utoipa's OpenAPI 3.1 to the 3.0 subset progenitor reads (ADR-0009). Only the
//! constructs utoipa emits are rewritten; anything else is left for progenitor to reject.

use serde_json::{Map, Value};

/// The 3.0 patch version we target — the one `openapiv3` is built against.
const TARGET_VERSION: &str = "3.0.3";

/// Rewrite `document` in place, returning how many nodes changed (so a no-op is visible).
pub(crate) fn to_openapi_30(document: &mut Value) -> usize {
    let mut changes = 0;

    if let Some(object) = document.as_object_mut() {
        object.insert("openapi".to_owned(), Value::from(TARGET_VERSION));
        changes += 1;
        // `$schema` and `webhooks` are 3.1-only top-level keys with no 3.0 meaning.
        for key in ["$schema", "webhooks"] {
            if object.remove(key).is_some() {
                changes += 1;
            }
        }
    }

    changes + walk(document) + unnullify_parameters(document)
}

/// Drop `nullable` from parameter schemas: progenitor would generate an uncompilable
/// `Option<Option<&str>>`, and `required: false` already says it.
fn unnullify_parameters(document: &mut Value) -> usize {
    let mut changes = 0;

    let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) else {
        return 0;
    };

    for path in paths.values_mut() {
        let Some(operations) = path.as_object_mut() else {
            continue;
        };
        for operation in operations.values_mut() {
            let Some(parameters) = operation
                .get_mut("parameters")
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            for parameter in parameters {
                let removed = parameter
                    .get_mut("schema")
                    .and_then(Value::as_object_mut)
                    .and_then(|schema| schema.remove("nullable"));
                if removed.is_some() {
                    changes += 1;
                }
            }
        }
    }

    changes
}

fn walk(node: &mut Value) -> usize {
    match node {
        Value::Object(object) => {
            let mut changes = lower_nullable_type(object)
                + lower_nullable_composition(object)
                + lower_const(object)
                + lower_exclusive_bounds(object);
            for value in object.values_mut() {
                changes += walk(value);
            }
            changes
        }
        Value::Array(items) => items.iter_mut().map(walk).sum(),
        _ => 0,
    }
}

/// 3.1 expresses an optional value as `"type": ["string", "null"]`.
/// 3.0 has no type unions: it is `"type": "string"` plus `"nullable": true`.
fn lower_nullable_type(object: &mut Map<String, Value>) -> usize {
    let Some(Value::Array(variants)) = object.get("type") else {
        return 0;
    };

    let nullable = variants.iter().any(|variant| variant == "null");
    let concrete: Vec<&Value> = variants
        .iter()
        .filter(|variant| *variant != "null")
        .collect();

    // A genuine two-type union has no 3.0 equivalent; let progenitor report it.
    let [single] = concrete.as_slice() else {
        return 0;
    };
    let single = (*single).clone();

    object.insert("type".to_owned(), single);
    if nullable {
        object.insert("nullable".to_owned(), Value::Bool(true));
    }
    1
}

/// `oneOf: [{"type": "null"}, {"$ref": …}]` (what utoipa emits for `Option<Ref>`) becomes
/// the inlined branch plus `nullable`; other unions with null are left for progenitor.
fn lower_nullable_composition(object: &mut Map<String, Value>) -> usize {
    for keyword in ["oneOf", "anyOf"] {
        let Some(Value::Array(branches)) = object.get(keyword) else {
            continue;
        };
        if branches.len() != 2 {
            continue;
        }

        let is_null = |branch: &Value| branch.get("type").is_some_and(|kind| kind == "null");
        let Some(position) = branches.iter().position(is_null) else {
            continue;
        };

        let survivor = branches[1 - position].clone();
        object.remove(keyword);

        // 3.0 tooling ignores `$ref` siblings, so the branch is inlined wholesale.
        if let Value::Object(fields) = survivor {
            for (key, value) in fields {
                object.insert(key, value);
            }
        }
        object.insert("nullable".to_owned(), Value::Bool(true));
        return 1;
    }
    0
}

/// 3.1's `const: X` is 3.0's single-valued `enum: [X]`.
fn lower_const(object: &mut Map<String, Value>) -> usize {
    let Some(value) = object.remove("const") else {
        return 0;
    };
    object.insert("enum".to_owned(), Value::Array(vec![value]));
    1
}

/// 3.1 makes the exclusive bounds numeric (`exclusiveMinimum: 3`); 3.0 makes them
/// booleans qualifying `minimum`/`maximum`.
fn lower_exclusive_bounds(object: &mut Map<String, Value>) -> usize {
    let mut changes = 0;
    for (exclusive, inclusive) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        let Some(bound) = object
            .get(exclusive)
            .filter(|value| value.is_number())
            .cloned()
        else {
            continue;
        };
        object.insert(inclusive.to_owned(), bound);
        object.insert(exclusive.to_owned(), Value::Bool(true));
        changes += 1;
    }
    changes
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "assertions in tests are meant to panic")]
mod tests {
    use super::*;

    #[test]
    fn optional_field_becomes_nullable() {
        let mut document = serde_json::json!({
            "openapi": "3.1.0",
            "components": { "schemas": { "S": { "properties": {
                "detail": { "type": ["string", "null"] }
            }}}}
        });

        to_openapi_30(&mut document);

        let detail = &document["components"]["schemas"]["S"]["properties"]["detail"];
        assert_eq!(detail["type"], "string");
        assert_eq!(detail["nullable"], true);
        assert_eq!(document["openapi"], TARGET_VERSION);
    }

    #[test]
    fn plain_type_is_left_alone() {
        let mut document = serde_json::json!({ "type": "string" });
        to_openapi_30(&mut document);
        assert_eq!(document["type"], "string");
        assert!(document.get("nullable").is_none());
    }

    #[test]
    fn union_of_two_concrete_types_is_not_guessed_at() {
        // No 3.0 equivalent — leave it so progenitor complains rather than us.
        let mut document = serde_json::json!({ "type": ["string", "integer"] });
        to_openapi_30(&mut document);
        assert!(document["type"].is_array());
    }

    #[test]
    fn an_optional_ref_loses_its_null_branch_and_gains_nullable() {
        // What utoipa emits for `Option<HumanDuration>`.
        let mut document = serde_json::json!({
            "oneOf": [
                { "type": "null" },
                { "$ref": "#/components/schemas/HumanDuration" }
            ]
        });

        to_openapi_30(&mut document);

        assert_eq!(document["$ref"], "#/components/schemas/HumanDuration");
        assert_eq!(document["nullable"], true);
        assert!(document.get("oneOf").is_none());
    }

    #[test]
    fn a_composition_without_a_null_branch_is_left_alone() {
        // A real sum type: dropping a branch here would change the API.
        let mut document = serde_json::json!({
            "oneOf": [{ "$ref": "#/a" }, { "$ref": "#/b" }]
        });

        to_openapi_30(&mut document);

        assert_eq!(document["oneOf"].as_array().unwrap().len(), 2);
        assert!(document.get("nullable").is_none());
    }

    #[test]
    fn an_optional_parameter_is_optional_rather_than_nullable() {
        let mut document = serde_json::json!({
            "paths": { "/notifications": { "post": { "parameters": [{
                "name": "Idempotency-Key",
                "in": "header",
                "required": false,
                "schema": { "type": ["string", "null"] }
            }]}}}
        });

        to_openapi_30(&mut document);

        let schema = &document["paths"]["/notifications"]["post"]["parameters"][0]["schema"];
        assert_eq!(schema["type"], "string");
        assert!(
            schema.get("nullable").is_none(),
            "a header is sent or not sent; there is no null form of it"
        );
        assert_eq!(
            document["paths"]["/notifications"]["post"]["parameters"][0]["required"], false,
            "optionality is `required: false`, and it has to survive"
        );
    }

    #[test]
    fn const_becomes_single_valued_enum() {
        let mut document = serde_json::json!({ "const": "revision" });
        to_openapi_30(&mut document);
        assert_eq!(document["enum"], serde_json::json!(["revision"]));
        assert!(document.get("const").is_none());
    }

    #[test]
    fn numeric_exclusive_bound_becomes_boolean_plus_bound() {
        let mut document = serde_json::json!({ "exclusiveMinimum": 0 });
        to_openapi_30(&mut document);
        assert_eq!(document["minimum"], 0);
        assert_eq!(document["exclusiveMinimum"], true);
    }
}
