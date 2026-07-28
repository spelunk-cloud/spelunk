// Golden-schema checker for the plumbing JSONL stability contract.
//
// Included with `mod schema_contract;` by the contract test binaries. Compiled
// standalone as its own (test-free) integration target too, which is why the
// dead-code allow is needed.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

// The committed contract. Field *presence and type* only: values vary run to
// run (line numbers, hashes, timestamps), so pinning them would make the
// contract a flake rather than a guarantee.
pub const GOLDEN_RELATIVE_PATH: &str = "tests/golden/plumbing_jsonl_schema.json";

pub fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_RELATIVE_PATH)
}

// One command's declared output shape.
//
// `required` fields must appear on every emitted line. `optional` fields are
// those a serializer may omit (`skip_serializing_if`) or that only occur on one
// of several outcome shapes; when present they must still match their type.
#[derive(Debug, Clone)]
pub struct CommandSchema {
    pub required: BTreeMap<String, FieldType>,
    pub optional: BTreeMap<String, FieldType>,
}

// A declared type, parsed from the golden file's compact spelling:
// `"string"`, `"string|null"`, `"array<number>"`, `"array<any>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldType {
    spelling: String,
    alternatives: Vec<Alternative>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Alternative {
    Scalar(Scalar),
    Array(Scalar),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scalar {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    Null,
    Any,
}

impl Scalar {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "string" => Ok(Scalar::String),
            "integer" => Ok(Scalar::Integer),
            "number" => Ok(Scalar::Number),
            "boolean" => Ok(Scalar::Boolean),
            "object" => Ok(Scalar::Object),
            "null" => Ok(Scalar::Null),
            "any" => Ok(Scalar::Any),
            other => Err(format!("unknown type name {other:?}")),
        }
    }

    fn matches(self, value: &serde_json::Value) -> bool {
        match self {
            Scalar::Any => true,
            Scalar::String => value.is_string(),
            // An integral JSON number is a valid `number`, but a fractional one
            // is not a valid `integer`: widening a count to a float is exactly
            // the kind of breaking change this contract exists to catch.
            Scalar::Integer => value.is_i64() || value.is_u64(),
            Scalar::Number => value.is_number(),
            Scalar::Boolean => value.is_boolean(),
            Scalar::Object => value.is_object(),
            Scalar::Null => value.is_null(),
        }
    }

    fn example(self) -> serde_json::Value {
        match self {
            Scalar::String => serde_json::json!("x"),
            Scalar::Integer => serde_json::json!(1),
            Scalar::Number => serde_json::json!(1.5),
            Scalar::Boolean => serde_json::json!(true),
            Scalar::Object => serde_json::json!({}),
            Scalar::Null | Scalar::Any => serde_json::Value::Null,
        }
    }
}

impl FieldType {
    pub fn parse(spelling: &str) -> Result<Self, String> {
        let mut alternatives = Vec::new();
        for part in spelling.split('|') {
            let part = part.trim();
            let alt = match part
                .strip_prefix("array<")
                .and_then(|r| r.strip_suffix('>'))
            {
                Some(inner) => Alternative::Array(Scalar::parse(inner.trim())?),
                None => Alternative::Scalar(Scalar::parse(part)?),
            };
            alternatives.push(alt);
        }
        if alternatives.is_empty() {
            return Err(format!("empty type spelling {spelling:?}"));
        }
        Ok(FieldType {
            spelling: spelling.to_string(),
            alternatives,
        })
    }

    pub fn spelling(&self) -> &str {
        &self.spelling
    }

    pub fn matches(&self, value: &serde_json::Value) -> bool {
        self.alternatives.iter().any(|alt| match alt {
            Alternative::Scalar(s) => s.matches(value),
            Alternative::Array(item) => match value.as_array() {
                Some(items) => items.iter().all(|i| item.matches(i)),
                None => false,
            },
        })
    }

    // Some value this declaration accepts, so a conforming row can be built from
    // the contract alone rather than from whatever a command happens to emit.
    pub fn example(&self) -> serde_json::Value {
        let value = match &self.alternatives[0] {
            Alternative::Scalar(s) => s.example(),
            Alternative::Array(item) => serde_json::Value::Array(vec![item.example()]),
        };
        assert!(
            self.matches(&value),
            "example for {:?} does not satisfy it: {value}",
            self.spelling
        );
        value
    }

    // Some value this declaration rejects, for driving the retype mutation.
    // `None` when the declaration accepts everything, which is itself worth
    // knowing: such a field is declared but unguarded.
    pub fn counterexample(&self) -> Option<serde_json::Value> {
        [
            serde_json::json!("a string"),
            serde_json::json!(7),
            serde_json::json!(7.5),
            serde_json::json!(true),
            serde_json::json!(null),
            serde_json::json!({"nested": 1}),
            serde_json::json!([{"nested": 1}]),
        ]
        .into_iter()
        .find(|candidate| !self.matches(candidate))
    }
}

fn parse_fields(
    command: &str,
    section: &str,
    value: Option<&serde_json::Value>,
) -> BTreeMap<String, FieldType> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let obj = value.as_object().unwrap_or_else(|| {
        panic!("golden schema: {command}.{section} must be an object, got {value}")
    });
    obj.iter()
        .map(|(field, spelling)| {
            let spelling = spelling.as_str().unwrap_or_else(|| {
                panic!("golden schema: {command}.{section}.{field} must be a type string")
            });
            let ty = FieldType::parse(spelling)
                .unwrap_or_else(|e| panic!("golden schema: {command}.{section}.{field}: {e}"));
            (field.clone(), ty)
        })
        .collect()
}

pub fn load_golden() -> BTreeMap<String, CommandSchema> {
    let path = golden_path();
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading golden schema {}: {e}", path.display()));
    parse_golden(&raw)
}

pub fn parse_golden(raw: &str) -> BTreeMap<String, CommandSchema> {
    let doc: serde_json::Value = serde_json::from_str(raw).expect("golden schema is valid JSON");
    let commands = doc
        .get("commands")
        .and_then(|c| c.as_object())
        .expect("golden schema has a top-level `commands` object");

    commands
        .iter()
        .map(|(name, spec)| {
            let schema = CommandSchema {
                required: parse_fields(name, "required", spec.get("required")),
                optional: parse_fields(name, "optional", spec.get("optional")),
            };
            assert!(
                !schema.required.is_empty(),
                "golden schema: {name} declares no required fields, so it guarantees nothing"
            );
            for field in schema.optional.keys() {
                assert!(
                    !schema.required.contains_key(field),
                    "golden schema: {name}.{field} is both required and optional"
                );
            }
            (name.clone(), schema)
        })
        .collect()
}

// Build a row that satisfies a schema, using only what the contract declares.
//
// Deriving it from the contract instead of from real output is what lets a
// mutation sweep run over every declared command without needing nine live
// fixtures, and keeps the sweep honest: nothing here can be copied from what
// the code currently emits.
pub fn conforming_row(schema: &CommandSchema) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for (field, ty) in &schema.required {
        obj.insert(field.clone(), ty.example());
    }
    for (field, ty) in &schema.optional {
        obj.insert(field.clone(), ty.example());
    }
    serde_json::Value::Object(obj)
}

// A single contract violation, reported rather than panicked so one run can
// surface every problem at once instead of only the first.
#[derive(Debug, PartialEq, Eq)]
pub enum Violation {
    NotAnObject {
        line: usize,
    },
    MissingField {
        line: usize,
        field: String,
    },
    WrongType {
        line: usize,
        field: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::NotAnObject { line } => {
                write!(f, "line {line}: emitted value is not a JSON object")
            }
            Violation::MissingField { line, field } => write!(
                f,
                "line {line}: required field `{field}` is missing (removed or renamed?)"
            ),
            Violation::WrongType {
                line,
                field,
                expected,
                actual,
            } => write!(
                f,
                "line {line}: field `{field}` has type {actual}, contract declares {expected}"
            ),
        }
    }
}

fn json_type_name(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(_) => "boolean".to_string(),
        serde_json::Value::Number(n) if n.is_f64() => "number".to_string(),
        serde_json::Value::Number(_) => "integer".to_string(),
        serde_json::Value::String(_) => "string".to_string(),
        serde_json::Value::Array(items) => {
            let inner = items
                .first()
                .map(json_type_name)
                .unwrap_or_else(|| "any".to_string());
            format!("array<{inner}>")
        }
        serde_json::Value::Object(_) => "object".to_string(),
    }
}

// Check emitted JSONL rows against a command's declared schema.
//
// Unknown fields are deliberately accepted: the contract's evolution rule is
// additive-only, so a new field must not break a consumer or this check.
pub fn check_rows(schema: &CommandSchema, rows: &[serde_json::Value]) -> Vec<Violation> {
    let mut violations = Vec::new();
    for (idx, row) in rows.iter().enumerate() {
        let line = idx + 1;
        let Some(obj) = row.as_object() else {
            violations.push(Violation::NotAnObject { line });
            continue;
        };
        for (field, ty) in &schema.required {
            match obj.get(field) {
                None => violations.push(Violation::MissingField {
                    line,
                    field: field.clone(),
                }),
                Some(value) if !ty.matches(value) => violations.push(Violation::WrongType {
                    line,
                    field: field.clone(),
                    expected: ty.spelling().to_string(),
                    actual: json_type_name(value),
                }),
                Some(_) => {}
            }
        }
        for (field, ty) in &schema.optional {
            if let Some(value) = obj.get(field)
                && !ty.matches(value)
            {
                violations.push(Violation::WrongType {
                    line,
                    field: field.clone(),
                    expected: ty.spelling().to_string(),
                    actual: json_type_name(value),
                });
            }
        }
    }
    violations
}

// Assert conformance, failing with every violation listed.
pub fn assert_conforms(command: &str, schema: &CommandSchema, rows: &[serde_json::Value]) {
    assert!(
        !rows.is_empty(),
        "`{command}` emitted no JSONL rows, so its schema was never exercised"
    );
    let violations = check_rows(schema, rows);
    assert!(
        violations.is_empty(),
        "`spelunk plumbing {command}` output violates the JSONL stability contract \
         ({}):\n{}\n\nIf this change is intentional and additive, the contract file needs no \
         edit. If a field was removed, renamed, or retyped, that is a breaking change to a \
         stable surface.",
        GOLDEN_RELATIVE_PATH,
        violations
            .iter()
            .map(|v| format!("  - {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
