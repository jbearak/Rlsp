// lib.rs — Exposes internal modules for benchmarks and integration tests.
//
// The main binary entry point remains in main.rs. This file re-exports
// the subset of modules that benches/ and tests/ need.
//
// NOTE: Only modules required by benchmarks/tests are included here.
// The full module set lives in main.rs (the binary crate root).

pub mod cli;
pub mod cross_file;
pub mod parser_pool;
pub mod reserved_words;
mod stan;
pub mod state;
// test_utils is available in test builds and when the `test-support` feature is enabled.
// This allows benchmarks and integration tests to import directly instead of #[path] hacks.
#[cfg(any(test, feature = "test-support"))]
pub mod test_utils;

// Transitive dependencies of the above modules:
pub mod backend;
pub mod box_use;
pub mod builtins;
pub mod chunks;
pub mod completion_context;
pub mod config_file;
pub mod content_provider;
pub mod diagnostic_code;
pub mod extract_op;
pub mod file_path_intellisense;
pub mod file_type;
pub mod foreach;
pub mod handlers;
pub mod help;
pub mod import_pkg;
pub mod indentation;
pub mod jags;
pub mod jags_builtins;
pub mod libpath_watcher;
pub mod linting;
pub mod namespace_completion;
pub mod namespace_parser;
pub mod nse;
pub(crate) mod open_document_store;
pub mod package_db;
pub mod package_library;
pub mod package_namespace;
pub mod package_state;
pub mod parameter_resolver;
pub mod perf;
pub mod qualified_resolve;
pub mod r_names;
pub mod r_subprocess;
pub mod roxygen;
pub mod selective_import;
pub mod stan_builtins;
pub mod utf16;
pub mod workspace_index;
