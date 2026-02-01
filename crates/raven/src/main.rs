//
// main.rs
//
// Copyright (C) 2022-2025 Posit Software, PBC. All rights reserved.
// Modifications copyright (C) 2026 Jonathan Marc Bearak
//

mod backend;
mod builtins;
mod content_provider;
mod cross_file;
mod document_store;
mod handlers;
mod help;
mod namespace_parser;
mod package_library;
mod parser_pool;
mod r_env;
mod r_subprocess;
mod state;
mod workspace_index;

use std::env;

fn print_usage() {
    println!("raven {}, a static R Language Server.", env!("CARGO_PKG_VERSION"));
    print!(
        r#"
Usage: raven [OPTIONS]

Available options:

--stdio                      Start the LSP server using stdio transport
--version                    Print the version
--help                       Print this help message

"#
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut argv = env::args();
    argv.next(); // skip executable name

    let mut use_stdio = false;

    for arg in argv {
        match arg.as_str() {
            "--stdio" => use_stdio = true,
            "--version" => {
                println!("raven {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" => {
                print_usage();
                return Ok(());
            }
            other => {
                return Err(anyhow::anyhow!("Unknown argument: '{other}'"));
            }
        }
    }

    if !use_stdio {
        print_usage();
        return Ok(());
    }

    env_logger::init();

    backend::start_lsp().await
}
