//! Proptest verifying the safety property of `derive_package_state`:
//! diff-driven derivation must equal from-scratch derivation.

#![cfg(test)]

use super::*;
use crate::cross_file::config::PackageMode;
use proptest::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug)]
enum Mutation {
    SetRFile {
        path: PathBuf,
        kind: RFileKind,
        text: String,
    },
    DeleteRFile {
        path: PathBuf,
    },
    SetNamespace {
        text: String,
    },
    SetDescription {
        text: String,
    },
    SetMode {
        mode: PackageMode,
    },
}

fn arb_path() -> impl Strategy<Value = (PathBuf, RFileKind)> {
    prop_oneof![
        "[a-z]{1,4}".prop_map(|n| (
            PathBuf::from(format!("/work/pkg/R/{}.R", n)),
            RFileKind::Source
        )),
        "[a-z]{1,4}".prop_map(|n| (
            PathBuf::from(format!("/work/pkg/tests/testthat/test-{}.R", n)),
            RFileKind::Test
        )),
        "[a-z]{1,4}".prop_map(|n| (
            PathBuf::from(format!("/work/pkg/tests/testit/test-{}.R", n)),
            RFileKind::Test
        )),
    ]
}

fn arb_r_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("foo <- function() 1\n".to_string()),
        Just("#' @export\nbar <- function(x) x + 1\n".to_string()),
        Just("#' @importFrom dplyr filter\nbaz <- function() 0\n".to_string()),
        Just("# empty\n".to_string()),
    ]
}

fn arb_mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (arb_path(), arb_r_text())
            .prop_map(|((path, kind), text)| { Mutation::SetRFile { path, kind, text } }),
        arb_path().prop_map(|(path, _)| Mutation::DeleteRFile { path }),
        prop_oneof![
            Just("import(magrittr)\n".to_string()),
            Just("importFrom(dplyr, filter)\n".to_string()),
            Just("".to_string()),
        ]
        .prop_map(|text| Mutation::SetNamespace { text }),
        prop_oneof![
            Just("Package: foo\n".to_string()),
            Just("Package: bar\n".to_string()),
            Just("# no package\n".to_string()),
        ]
        .prop_map(|text| Mutation::SetDescription { text }),
        prop_oneof![
            Just(PackageMode::Auto),
            Just(PackageMode::Enabled),
            Just(PackageMode::Disabled),
        ]
        .prop_map(|mode| Mutation::SetMode { mode }),
    ]
}

fn apply(inputs: &mut PackageInputs, mutation: &Mutation) -> PackageInputDelta {
    match mutation {
        Mutation::SetRFile { path, kind, text } => {
            let arc: Arc<str> = text.as_str().into();
            let digest = ContentDigest::of(&arc);
            inputs.r_files.insert(
                path.clone(),
                RFileInput {
                    kind: *kind,
                    text: arc,
                    content_digest: digest,
                },
            );
            PackageInputDelta::RFileChanged {
                path: path.clone(),
                kind: *kind,
            }
        }
        Mutation::DeleteRFile { path } => {
            if let Some(prev) = inputs.r_files.remove(path) {
                PackageInputDelta::RFileDeleted {
                    path: path.clone(),
                    kind: prev.kind,
                }
            } else {
                // Absent-path deletion is a no-op: no inputs changed. An empty
                // `Batch` expresses that exactly, without conflating the
                // situation with `Initial` (a fresh activation) or forcing a
                // new enum variant. The delta is advisory — see the doc
                // comment on `derive_package_state` — so this choice only
                // affects how observers reading the delta interpret the
                // event.
                PackageInputDelta::Batch(Vec::new())
            }
        }
        Mutation::SetNamespace { text } => {
            inputs.namespace = if text.is_empty() {
                None
            } else {
                Some(NamespaceInput {
                    text: text.as_str().into(),
                })
            };
            PackageInputDelta::NamespaceChanged
        }
        Mutation::SetDescription { text } => {
            inputs.description = Some(DescriptionInput {
                text: text.as_str().into(),
            });
            PackageInputDelta::DescriptionChanged
        }
        Mutation::SetMode { mode } => {
            inputs.package_mode = *mode;
            PackageInputDelta::SettingChanged
        }
    }
}

fn initial_inputs() -> PackageInputs {
    PackageInputs {
        workspace_root: Some("/work/pkg".into()),
        package_mode: PackageMode::Auto,
        description: Some(DescriptionInput {
            text: "Package: foo\n".into(),
        }),
        namespace: None,
        r_files: BTreeMap::new(),
        dataset_names: BTreeSet::new(),
        sysdata_names: BTreeSet::new(),
        model_rprofile: false,
        rprofile_symbols: BTreeSet::new(),
        rprofile_attached_packages: BTreeSet::new(),
        rprofile_sourced_files: BTreeSet::new(),
        preamble_sourced_symbols: BTreeMap::new(),
        preamble_sourced_attached_packages: BTreeMap::new(),
        preamble_sourced_files: BTreeSet::new(),
        preamble_sourced_files_by_preamble: BTreeMap::new(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn diff_driven_equals_recompute(mutations in proptest::collection::vec(arb_mutation(), 1..16)) {
        let mut inputs = initial_inputs();
        let mut state = PackageState::default();
        for m in &mutations {
            let delta = apply(&mut inputs, m);
            state = derive_package_state(&state, &inputs, &delta);
        }
        let from_scratch = derive_package_state(
            &PackageState::default(),
            &inputs,
            &PackageInputDelta::Initial,
        );
        // Compare all derived fields.
        prop_assert_eq!(&state.workspace, &from_scratch.workspace);
        prop_assert_eq!(&state.namespace_model, &from_scratch.namespace_model);
        prop_assert_eq!(&state.r_file_facts, &from_scratch.r_file_facts);
        prop_assert_eq!(&state.scope_contribution, &from_scratch.scope_contribution);
    }

    #[test]
    fn advisory_delta_does_not_affect_correctness(mutations in proptest::collection::vec(arb_mutation(), 1..8)) {
        let mut inputs = initial_inputs();
        let mut s_correct = PackageState::default();
        let mut s_wrong = PackageState::default();
        for m in &mutations {
            let delta = apply(&mut inputs, m);
            s_correct = derive_package_state(&s_correct, &inputs, &delta);
            // Use Initial as the "wrong/advisory" delta — the result must still match.
            s_wrong = derive_package_state(&s_wrong, &inputs, &PackageInputDelta::Initial);
        }
        prop_assert_eq!(&s_correct.r_file_facts, &s_wrong.r_file_facts);
        prop_assert_eq!(&s_correct.scope_contribution, &s_wrong.scope_contribution);
    }
}
