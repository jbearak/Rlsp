//! Load `raven.toml` into a `serde_json::Value` shaped exactly like the LSP
//! `initializationOptions` payload. Unknown keys produce a warning but do not
//! abort the load.

use std::path::Path;

use serde_json::Value;

/// Outcome of a TOML-load attempt.
pub struct LoadedToml {
    /// The decoded settings as JSON, ready to feed `parse_*_config` after
    /// merging with client settings.
    pub settings: Value,
    /// Warning messages collected during load. Caller should log each.
    pub warnings: Vec<String>,
}

/// Read `path` as TOML and convert into project-shape JSON. Returns `None`
/// if the file cannot be read or parsed; warnings are still collected when a
/// recoverable schema issue is encountered (unknown keys).
pub fn load(path: &Path) -> Option<LoadedToml> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("raven.toml: cannot read {}: {}", path.display(), e);
            return None;
        }
    };
    load_str(&text, &path.display().to_string())
}

/// Pure variant for testing.
pub fn load_str(text: &str, source_label: &str) -> Option<LoadedToml> {
    let toml_value: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("{source_label}: malformed TOML: {e}");
            return None;
        }
    };
    let json = toml_to_json(toml_value);
    let mut warnings = Vec::new();
    if let Value::Object(map) = &json {
        validate_top_level_keys(map, source_label, &mut warnings);
    } else {
        warnings.push(format!("{source_label}: top-level value must be a table"));
        return Some(LoadedToml {
            settings: Value::Object(serde_json::Map::new()),
            warnings,
        });
    }
    Some(LoadedToml {
        settings: json,
        warnings,
    })
}

/// Recursive TOML → JSON conversion. TOML's date/time types are stringified
/// (we don't expect them in Raven's schema; this keeps the loader total).
fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => {
            let map: serde_json::Map<String, Value> = table
                .into_iter()
                .map(|(k, v)| (k, toml_to_json(v)))
                .collect();
            Value::Object(map)
        }
    }
}

const KNOWN_TOP_LEVEL: &[&str] = &[
    "linting",
    "crossFile",
    "packages",
    "diagnostics",
    "indentation",
    "symbols",
    "completion",
    "workspace",
];

/// Known project-scoped leaves under `[linting]`. **Hand-maintained**: when
/// adding a new `raven.linting.*` setting with a portable `raven.toml`
/// equivalent, update this list AND the schema in
/// `editors/vscode/src/initializationOptions.ts` AND the parser in
/// `crates/raven/src/backend.rs::parse_lint_config`. Client-only linting
/// settings such as `raven.linting.readHomeLintr` stay out of this list.
/// Forgetting the list here causes a spurious "unknown key" warning at load
/// time; forgetting the parser causes the new setting to be silently ignored.
const KNOWN_LINTING_KEYS: &[&str] = &[
    "enabled",
    "lineLength",
    "objectLength",
    "indentationUnit",
    "assignmentOperator",
    "stringDelimiter",
    "objectNameStyleFunction",
    "objectNameStyleVariable",
    "objectNameStyleArgument",
    "lineLengthSeverity",
    "trailingWhitespaceSeverity",
    "noTabSeverity",
    "trailingBlankLinesSeverity",
    "assignmentOperatorSeverity",
    "objectNameSeverity",
    "infixSpacesSeverity",
    "commentedCodeSeverity",
    "quotesSeverity",
    "commasSeverity",
    "tAndFSymbolSeverity",
    "semicolonSeverity",
    "equalsNaSeverity",
    "objectLengthSeverity",
    "vectorLogicSeverity",
    "functionLeftParenthesesSeverity",
    "spacesInsideSeverity",
    "indentationSeverity",
    "overrides",
];

/// For nested validation we accept the existence of any key in a known
/// section but warn on unknown leaves. The exhaustive nested key lists live
/// at the call sites of `parse_*_config` in `backend.rs`; for v1 we validate
/// `[linting]` (the most user-facing section) and trust the parsers to
/// ignore unrecognized keys in the other sections quietly.
fn validate_top_level_keys(
    map: &serde_json::Map<String, Value>,
    source_label: &str,
    warnings: &mut Vec<String>,
) {
    for (key, value) in map {
        if !KNOWN_TOP_LEVEL.contains(&key.as_str()) {
            warnings.push(format!(
                "{source_label}: unknown top-level key '{key}'; ignoring"
            ));
            continue;
        }
        if key == "linting"
            && let Value::Object(linting_map) = value
        {
            for nested in linting_map.keys() {
                if !KNOWN_LINTING_KEYS.contains(&nested.as_str()) {
                    warnings.push(format!(
                        "{source_label}: unknown key 'linting.{nested}'; ignoring"
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linting_section() {
        let toml = r#"
[linting]
enabled = true
lineLength = 100
lineLengthSeverity = "warning"
"#;
        let out = load_str(toml, "test").unwrap();
        assert_eq!(out.warnings, Vec::<String>::new());
        let linting = out.settings.get("linting").unwrap();
        assert_eq!(linting["enabled"], serde_json::json!(true));
        assert_eq!(linting["lineLength"], serde_json::json!(100));
        assert_eq!(linting["lineLengthSeverity"], serde_json::json!("warning"));
    }

    #[test]
    fn parses_nested_crossfile_section() {
        let toml = r#"
[crossFile.onDemandIndexing]
enabled = true
"#;
        let out = load_str(toml, "test").unwrap();
        let on_demand = &out.settings["crossFile"]["onDemandIndexing"];
        assert_eq!(on_demand["enabled"], serde_json::json!(true));
    }

    #[test]
    fn parses_overrides_as_array() {
        let toml = r#"
[linting]
lineLength = 80

[[linting.overrides]]
files = ["tests/**/*.R"]
lineLength = 120

[[linting.overrides]]
files = ["R/legacy_*.R"]
enabled = false
"#;
        let out = load_str(toml, "test").unwrap();
        let overrides = out.settings["linting"]["overrides"].as_array().unwrap();
        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides[0]["lineLength"], serde_json::json!(120));
        assert_eq!(overrides[1]["enabled"], serde_json::json!(false));
    }

    #[test]
    fn unknown_top_level_keys_produce_warning() {
        let toml = r#"
[linting]
enabled = true

[bogusSection]
foo = 1
"#;
        let out = load_str(toml, "test").unwrap();
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("bogusSection"));
    }

    #[test]
    fn unknown_nested_linting_key_produces_warning() {
        let toml = r#"
[linting]
enabled = true
foo = 42
"#;
        let out = load_str(toml, "test").unwrap();
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("linting.foo"));
    }

    #[test]
    fn malformed_toml_returns_none() {
        let toml = "this is not = valid = toml = at all";
        assert!(load_str(toml, "test").is_none());
    }
}
