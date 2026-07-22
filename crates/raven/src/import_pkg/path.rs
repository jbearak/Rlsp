//! Exact literal script-module path resolution for `{import}`.

use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::Url;

use super::{ImportCall, ImportSpec, LocalScriptResolution};
use crate::cross_file::path_resolve::{
    CaseMismatchRegime, PathContext, forward_path_candidate_tiers, resolve_source_path_rich,
};

pub(crate) fn enrich_local_imports(
    importing_uri: &Url,
    metadata: &crate::cross_file::CrossFileMetadata,
    imports: &mut [ImportCall],
    workspace_root: Option<&Url>,
) {
    let context = PathContext::from_metadata(importing_uri, metadata, workspace_root);
    for import in imports {
        let ImportSpec::LocalScript { path, directory } = &import.spec else {
            import.local_resolution = None;
            continue;
        };
        let requested = requested_path(path, directory.as_deref());
        import.local_resolution = Some(match context.as_ref() {
            Some(context) => {
                let context = import_path_context(&requested, context);
                let outcome = resolve_source_path_rich(&requested, &context);
                match (outcome.path, outcome.case_mismatch) {
                    (Some(found), Some(regime)) if found.is_file() => {
                        let expected = candidate_paths(&requested, &context)
                            .into_iter()
                            .find(|candidate| {
                                candidate
                                    .to_string_lossy()
                                    .eq_ignore_ascii_case(&found.to_string_lossy())
                            })
                            .unwrap_or_else(|| found.clone());
                        LocalScriptResolution::CaseMismatch {
                            expected,
                            found,
                            case_sensitive_fs: matches!(
                                regime,
                                CaseMismatchRegime::CaseSensitiveFs
                            ),
                        }
                    }
                    (Some(path), None) if path.is_file() => Url::from_file_path(path).ok().map_or(
                        LocalScriptResolution::Missing,
                        LocalScriptResolution::Resolved,
                    ),
                    _ => LocalScriptResolution::Missing,
                }
            }
            None => LocalScriptResolution::Missing,
        });
    }
}

pub(crate) fn candidate_set_matches_path(
    importing_uri: &Url,
    metadata: &crate::cross_file::CrossFileMetadata,
    import: &ImportCall,
    workspace_root: Option<&Url>,
    changed_path: &Path,
) -> bool {
    let ImportSpec::LocalScript { path, directory } = &import.spec else {
        return false;
    };
    let requested = requested_path(path, directory.as_deref());
    let Some(context) = PathContext::from_metadata(importing_uri, metadata, workspace_root) else {
        return false;
    };
    let context = import_path_context(&requested, &context);
    let changed = changed_path.to_string_lossy();
    candidate_paths(&requested, &context)
        .iter()
        .any(|candidate| {
            candidate == changed_path
                || candidate
                    .to_string_lossy()
                    .eq_ignore_ascii_case(changed.as_ref())
        })
}

fn requested_path(path: &str, directory: Option<&str>) -> String {
    if Path::new(path).is_absolute() {
        path.to_string()
    } else {
        directory.map_or_else(
            || path.to_string(),
            |directory| format!("{directory}/{path}"),
        )
    }
}

fn import_path_context(path: &str, context: &PathContext) -> PathContext {
    let mut context = context.clone();
    // Raven's shared forward resolver interprets a leading `/` as
    // workspace-root-relative for source()-style paths. `{import}` instead uses
    // ordinary filesystem absolute paths, so anchor that resolver tier at the
    // filesystem root while retaining its exact/case-mismatch behavior.
    if Path::new(path).is_absolute() && path.starts_with(std::path::MAIN_SEPARATOR) {
        context.workspace_root = Some(PathBuf::from(std::path::MAIN_SEPARATOR.to_string()));
    }
    context
}

fn candidate_paths(path: &str, context: &PathContext) -> Vec<PathBuf> {
    forward_path_candidate_tiers(path, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_call(path: &str) -> ImportCall {
        ImportCall {
            spec: ImportSpec::LocalScript {
                path: path.to_string(),
                directory: None,
            },
            local_resolution: None,
            attach: Vec::new(),
            destination: crate::selective_import::ImportDestination::CurrentEnvironment,
            excluded_exports: Default::default(),
            line: 0,
            column: 0,
            end_column: 10,
            source_line: 0,
            source_column: 0,
            source_end_column: path.len() as u32,
            function_scoped: false,
        }
    }

    #[test]
    fn resolves_exact_script_and_literal_directory_without_box_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let importer_path = dir.path().join("main.R");
        let module_dir = dir.path().join("modules");
        std::fs::create_dir(&module_dir).unwrap();
        let module_path = module_dir.join("helpers.R");
        std::fs::write(&importer_path, "").unwrap();
        std::fs::write(&module_path, "helper <- 1\n").unwrap();
        let importer = Url::from_file_path(importer_path).unwrap();
        let mut metadata = crate::cross_file::CrossFileMetadata::default();
        metadata.import_calls.push(ImportCall {
            spec: ImportSpec::LocalScript {
                path: "helpers.R".to_string(),
                directory: Some("modules".to_string()),
            },
            local_resolution: None,
            attach: Vec::new(),
            destination: crate::selective_import::ImportDestination::CurrentEnvironment,
            excluded_exports: Default::default(),
            line: 0,
            column: 0,
            end_column: 10,
            source_line: 0,
            source_column: 0,
            source_end_column: 9,
            function_scoped: false,
        });
        let snapshot = metadata.clone();
        enrich_local_imports(&importer, &snapshot, &mut metadata.import_calls, None);
        assert!(matches!(
            metadata.import_calls[0].local_resolution,
            Some(LocalScriptResolution::Resolved(ref uri)) if uri == &Url::from_file_path(&module_path).unwrap()
        ));

        assert!(candidate_set_matches_path(
            &importer,
            &snapshot,
            &metadata.import_calls[0],
            None,
            &module_dir.join("HELPERS.r")
        ));

        std::fs::remove_file(&module_path).unwrap();
        let snapshot = metadata.clone();
        enrich_local_imports(&importer, &snapshot, &mut metadata.import_calls, None);
        assert!(matches!(
            metadata.import_calls[0].local_resolution,
            Some(LocalScriptResolution::Missing)
        ));
        std::fs::write(&module_path, "helper <- 2\n").unwrap();
        let snapshot = metadata.clone();
        enrich_local_imports(&importer, &snapshot, &mut metadata.import_calls, None);
        assert!(matches!(
            metadata.import_calls[0].local_resolution,
            Some(LocalScriptResolution::Resolved(_))
        ));
    }

    #[test]
    fn absolute_script_path_ignores_literal_directory() {
        let dir = tempfile::tempdir().unwrap();
        let importer_path = dir.path().join("main.R");
        let module_path = dir.path().join("helpers.R");
        std::fs::write(&importer_path, "").unwrap();
        std::fs::write(&module_path, "helper <- 1\n").unwrap();
        let importer = Url::from_file_path(importer_path).unwrap();
        let mut metadata = crate::cross_file::CrossFileMetadata::default();
        metadata.import_calls.push(ImportCall {
            spec: ImportSpec::LocalScript {
                path: module_path.to_string_lossy().to_string(),
                directory: Some("ignored".to_string()),
            },
            ..local_call("unused.R")
        });
        let snapshot = metadata.clone();
        enrich_local_imports(&importer, &snapshot, &mut metadata.import_calls, None);
        assert!(matches!(
            metadata.import_calls[0].local_resolution,
            Some(LocalScriptResolution::Resolved(ref uri)) if uri == &Url::from_file_path(&module_path).unwrap()
        ));
        assert!(candidate_set_matches_path(
            &importer,
            &snapshot,
            &metadata.import_calls[0],
            None,
            &module_path,
        ));
    }

    #[test]
    fn records_case_only_matches_without_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let importer_path = dir.path().join("main.R");
        let module_path = dir.path().join("Helpers.R");
        std::fs::write(&importer_path, "").unwrap();
        std::fs::write(&module_path, "helper <- 1\n").unwrap();
        let importer = Url::from_file_path(importer_path).unwrap();
        let mut metadata = crate::cross_file::CrossFileMetadata::default();
        metadata.import_calls.push(local_call("helpers.R"));
        let snapshot = metadata.clone();
        enrich_local_imports(&importer, &snapshot, &mut metadata.import_calls, None);
        assert!(matches!(
            &metadata.import_calls[0].local_resolution,
            Some(LocalScriptResolution::CaseMismatch { found, .. }) if found == &module_path
        ));
        assert!(metadata.import_calls[0].resolved_source().is_none());
    }
}
