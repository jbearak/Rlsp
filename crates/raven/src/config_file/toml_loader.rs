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
    let mut json = toml_to_json(toml_value);
    let mut warnings = Vec::new();
    normalize_shared_paths(&mut json, source_label, &mut warnings);
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

/// Normalize the shared Raven/Sight TOML paths into Raven's existing LSP
/// settings shape. The old Raven paths remain permanent aliases, while the
/// canonical shared path wins when both are present.
fn normalize_shared_paths(json: &mut Value, source_label: &str, warnings: &mut Vec<String>) {
    normalize_alias(
        json,
        &["workspace", "exclude"],
        &["exclude"],
        &["workspace", "exclude"],
        source_label,
        warnings,
    );
    normalize_alias(
        json,
        &["diagnostics", "severity", "undefinedVariable"],
        &["diagnostics", "undefinedVariableSeverity"],
        &["diagnostics", "undefinedVariableSeverity"],
        source_label,
        warnings,
    );
    normalize_alias(
        json,
        &["crossFile", "diagnostics", "missingFile"],
        &["crossFile", "missingFileSeverity"],
        &["crossFile", "missingFileSeverity"],
        source_label,
        warnings,
    );
    normalize_alias(
        json,
        &["crossFile", "diagnostics", "caseMismatch"],
        &["crossFile", "caseMismatchSeverity"],
        &["crossFile", "caseMismatchSeverity"],
        source_label,
        warnings,
    );
}

fn normalize_alias(
    json: &mut Value,
    canonical_path: &[&str],
    alias_path: &[&str],
    internal_path: &[&str],
    source_label: &str,
    warnings: &mut Vec<String>,
) {
    let canonical = get_path(json, canonical_path).cloned();
    let alias = get_path(json, alias_path).cloned();
    let canonical_is_set = canonical.is_some();
    let alias_is_set = alias.is_some();

    let Some(value) = canonical.or(alias) else {
        return;
    };

    if canonical_is_set && alias_is_set {
        warnings.push(format!(
            "{source_label}: both '{}' and compatibility alias '{}' are set; using '{}'",
            canonical_path.join("."),
            alias_path.join("."),
            canonical_path.join(".")
        ));
    }

    if !set_path(json, internal_path, value.clone()) {
        if canonical_is_set {
            warnings.push(format!(
                "{source_label}: cannot apply '{}' because its parent is not a table",
                canonical_path.join(".")
            ));
            return;
        }

        warnings.push(format!(
            "{source_label}: parent of '{}' is not a table; replacing it to apply compatibility alias '{}'",
            canonical_path.join("."),
            alias_path.join(".")
        ));
        set_path_replacing_non_tables(json, internal_path, value)
            .expect("top-level TOML value was already validated as a table");
    }
    if canonical_path != internal_path {
        remove_path(json, canonical_path);
    }
    if alias_path != internal_path {
        remove_path(json, alias_path);
    }
}

fn set_path_replacing_non_tables(value: &mut Value, path: &[&str], new_value: Value) -> Option<()> {
    let (last, parents) = path.split_last()?;
    let mut current = value;
    for key in parents {
        let object = current.as_object_mut()?;
        current = object
            .entry((*key).to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !current.is_object() {
            *current = Value::Object(serde_json::Map::new());
        }
    }
    current
        .as_object_mut()?
        .insert((*last).to_string(), new_value);
    Some(())
}

fn get_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |current, key| current.get(key))
}

fn set_path(value: &mut Value, path: &[&str], new_value: Value) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut current = value;
    for key in parents {
        let Some(object) = current.as_object_mut() else {
            return false;
        };
        current = object
            .entry((*key).to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
    let Some(object) = current.as_object_mut() else {
        return false;
    };
    object.insert((*last).to_string(), new_value);
    true
}

fn remove_path(value: &mut Value, path: &[&str]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = value;
    for key in parents {
        let Some(next) = current.get_mut(key) else {
            return;
        };
        current = next;
    }
    if let Some(object) = current.as_object_mut() {
        object.remove(*last);
    }
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
    "infixContinuationStyle",
    "assignmentOperator",
    "stringDelimiter",
    "objectNameStyleFunction",
    "objectNameStyleVariable",
    "objectNameStyleArgument",
    "objectNameRegexesFunction",
    "objectNameRegexesVariable",
    "objectNameRegexesArgument",
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
    fn parses_indentation_axes_and_compatibility_alias() {
        let toml = r#"
[indentation]
enabled = true
argumentStyle = "off"
infixContinuationStyle = "indented"
style = "rstudio-minus"
"#;
        let out = load_str(toml, "test").unwrap();
        assert!(out.warnings.is_empty(), "got {:?}", out.warnings);
        assert_eq!(
            out.settings["indentation"]["enabled"],
            serde_json::json!(true)
        );
        assert_eq!(
            out.settings["indentation"]["argumentStyle"],
            serde_json::json!("off")
        );
        assert_eq!(
            out.settings["indentation"]["infixContinuationStyle"],
            serde_json::json!("indented")
        );
        assert_eq!(
            out.settings["indentation"]["style"],
            serde_json::json!("rstudio-minus")
        );
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
    fn parses_portable_syntax_diagnostic_cap() {
        let out = load_str("[diagnostics]\nmaxSyntaxDiagnosticsPerFile = 0\n", "test").unwrap();
        assert!(out.warnings.is_empty(), "got {:?}", out.warnings);
        assert_eq!(
            out.settings["diagnostics"]["maxSyntaxDiagnosticsPerFile"],
            serde_json::json!(0)
        );
    }

    #[test]
    fn parses_model_diagnostic_switches() {
        let out = load_str("[diagnostics]\njags = \"on\"\nstan = \"off\"\n", "test").unwrap();
        assert!(out.warnings.is_empty(), "got {:?}", out.warnings);
        assert_eq!(out.settings["diagnostics"]["jags"], "on");
        assert_eq!(out.settings["diagnostics"]["stan"], "off");
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

    #[test]
    fn normalizes_shared_canonical_paths() {
        let out = load_str(
            r#"
[workspace]
exclude = ["generated/**"]

[diagnostics.severity]
undefinedVariable = "error"

[crossFile.diagnostics]
missingFile = "off"
caseMismatch = "auto"
"#,
            "test",
        )
        .unwrap();

        assert_eq!(
            out.settings["workspace"]["exclude"],
            serde_json::json!(["generated/**"])
        );
        assert_eq!(
            out.settings["diagnostics"]["undefinedVariableSeverity"],
            "error"
        );
        assert_eq!(out.settings["crossFile"]["missingFileSeverity"], "off");
        assert_eq!(out.settings["crossFile"]["caseMismatchSeverity"], "auto");
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn accepts_legacy_shared_paths_and_canonical_wins_collisions() {
        let out = load_str(
            r#"
exclude = ["legacy/**"]

[workspace]
exclude = ["canonical/**"]

[diagnostics]
undefinedVariableSeverity = "warning"

[diagnostics.severity]
undefinedVariable = "error"

[crossFile]
missingFileSeverity = "warning"
caseMismatchSeverity = "warning"

[crossFile.diagnostics]
missingFile = "error"
caseMismatch = "off"
"#,
            "test",
        )
        .unwrap();

        assert_eq!(
            out.settings["workspace"]["exclude"],
            serde_json::json!(["canonical/**"])
        );
        assert_eq!(
            out.settings["diagnostics"]["undefinedVariableSeverity"],
            "error"
        );
        assert_eq!(out.settings["crossFile"]["missingFileSeverity"], "error");
        assert_eq!(out.settings["crossFile"]["caseMismatchSeverity"], "off");
        assert_eq!(out.warnings.len(), 4);
        assert!(out.warnings.iter().all(|warning| warning.contains("using")));
    }

    #[test]
    fn root_exclude_alias_reaches_workspace_exclusion_compiler() {
        let out = load_str(r#"exclude = ["generated/**"]"#, "test").unwrap();
        let root = std::path::PathBuf::from("/workspace");
        let exclusions =
            crate::config_file::compile_workspace_exclusions(&out.settings, vec![root.clone()]);

        assert!(out.warnings.is_empty());
        assert!(out.settings.get("exclude").is_none());
        assert!(exclusions.is_excluded_path(&root.join("generated/file.R")));
    }

    #[test]
    fn malformed_workspace_does_not_disable_root_exclude_alias() {
        let out = load_str(
            r#"
workspace = "bad"
exclude = ["generated/**"]
"#,
            "test",
        )
        .unwrap();

        assert_eq!(
            out.settings["workspace"]["exclude"],
            serde_json::json!(["generated/**"])
        );
        assert!(out.settings.get("exclude").is_none());
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("replacing it to apply compatibility alias 'exclude'"));
    }
}
