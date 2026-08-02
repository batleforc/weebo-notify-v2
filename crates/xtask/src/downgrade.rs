//! Lower an OpenAPI 3.1 document to the 3.0 subset progenitor understands.
//!
//! Why this exists: utoipa 5 can only emit 3.1 (its `OpenApiVersion` has a single
//! variant), while progenitor parses through `openapiv3`, which is 3.0. The two
//! differ in a handful of JSON-Schema constructs, all of which have an exact 3.0
//! equivalent — so the conversion is lossless for the shapes we emit, not a
//! best-effort approximation. See ADR-0009.
//!
//! Deliberately narrow: it rewrites only what utoipa actually produces. Anything
//! else is left untouched so an unsupported construct fails loudly in progenitor
//! rather than being silently mangled here.

use serde_json::{Map, Value};

/// The 3.0 patch version we target — the one `openapiv3` is built against.
const TARGET_VERSION: &str = "3.0.3";

/// Rewrite `document` in place. Returns the number of nodes changed, which the
/// caller reports so a silent no-op is visible.
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

    changes + walk(document)
}

fn walk(node: &mut Value) -> usize {
    match node {
        Value::Object(object) => {
            let mut changes =
                lower_nullable_type(object) + lower_const(object) + lower_exclusive_bounds(object);
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

    // A genuine union of two non-null types has no 3.0 equivalent; leave it alone
    // so progenitor reports it instead of us guessing.
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
