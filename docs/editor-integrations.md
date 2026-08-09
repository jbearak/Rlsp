# Editor Integrations

Raven runs over stdio (`raven --stdio`) and works with any editor that has an LSP client.

## Getting the binary

The VS Code extension bundles the binary — no separate download needed. For all other editors, download a pre-built `raven` binary from the [GitHub releases page](https://github.com/jbearak/raven/releases) and place it on your PATH (or note the full path for the snippets below).

## VS Code

Install the extension (which bundles the binary) from the [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=jbearak.raven-r) or [OpenVSX](https://open-vsx.org/extension/jbearak/raven-r).

To use alongside [vscode-R](https://github.com/REditorSupport/vscode-R) (for running code, viewing plots, etc.), disable its language server to avoid duplicate completions:

```json
"r.lsp.enabled": false
```

You may also want:

```json
"editor.snippetSuggestions": "bottom"
```

## Zed

Add to your `settings.json`:

```json
"languages": {
  "R": {
    "language_servers": ["r_language_server"]
  }
},
"lsp": {
  "r_language_server": {
    "binary": {
      "path": "/path/to/raven",
      "arguments": ["--stdio"]
    }
  }
}
```

## Neovim

Manual setup (Neovim 0.8+):

```lua
vim.lsp.start({
  name = "raven",
  cmd = { "/path/to/raven", "--stdio" },
  filetypes = { "r", "rmd" },
  root_dir = vim.fs.dirname(vim.fs.find({ ".git" }, { upward = true })[1]),
})
```

## Generic LSP Client

Any LSP client that supports stdio transport:

```bash
raven --stdio
```

Configure your editor's LSP client to run this command for `.R`, `.r`, `.Rmd`, `.qmd`, `.jags`, `.bugs`, `.bug`, and `.stan` files. Raven classifies JAGS extensions case-insensitively and applies strict JAGS-dialect behavior; this does not claim general OpenBUGS, WinBUGS, MultiBUGS, or NIMBLE compatibility. Some editors apply case-sensitive file filters before launching Raven, so configure case-insensitive matching when available or enumerate the case variants your project uses.

## Agent Integration

### Claude Code

Install the `raven-lsp` plugin from the
[`jbearak/claude-plugins`](https://github.com/jbearak/claude-plugins)
marketplace:

```text
/plugin marketplace add jbearak/claude-plugins
/plugin install raven-lsp@jbearak
```

The plugin configures Claude Code to launch Raven for R files (`.R`, `.r`, `.Rmd`).

### OpenCode

Create `opencode.json` in your project root:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "lsp": {
    "r": {
      "command": ["raven", "--stdio"],
      "extensions": [".R", ".r", ".Rmd"]
    }
  }
}
```

### Kiro CLI

Create `lsp.json` in your project root:

```json
{
  "languages": {
    "r": {
      "name": "raven",
      "command": "raven",
      "args": ["--stdio"],
      "file_extensions": ["R", "r", "Rmd"],
      "project_patterns": [".Rproj"]
    }
  }
}
```

### Crush

Create `crush.json` in your project root:

```json
{
  "$schema": "https://charm.land/crush.json",
  "lsp": {
    "r": {
      "command": "raven",
      "args": ["--stdio"],
      "extensions": [".R", ".r", ".Rmd"]
    }
  }
}
```

## Project configuration

All editor integrations honor `raven.toml` at the project root automatically. See [Configuration § Project config](configuration.md#project-config-raventoml). Editor-specific settings (e.g. VS Code's `raven.linting.lineLength`) act as a fallback for keys the project file does not pin.

## Troubleshooting

- **Server not found**: Ensure `raven` is on your PATH, or specify the full path to the binary.
- **No diagnostics**: Check that R files have `.R` or `.r`, JAGS files have `.jags`, `.bugs`, or `.bug`, and Stan files have `.stan`. Model-file matching is case-insensitive. JAGS and Stan diagnostics default to off even though model files are still discovered: set `jags = "on"` and/or `stan = "on"` under `[diagnostics]` in `raven.toml`, or use the corresponding `raven.diagnostics.jags` / `raven.diagnostics.stan` VS Code settings. JAGS receives syntax-only diagnostics; Stan also reports conservative undeclared variables in files with real program blocks, while block-free fragments and files containing `#include` intentionally remain silent for that semantic pass.
- **Stale package completions**: Run **Raven: Refresh package cache** from the command palette, or restart the server.
- **Package watcher issues on Linux**: Raven watches `.libPaths()` recursively (~10–20 watches per installed package). On systems with the legacy 8192 inotify limit, the watcher can fail silently. Raise the limit: `sudo sysctl fs.inotify.max_user_watches=524288` (persist via `/etc/sysctl.conf`).
- **Cross-file features not working**: Ensure the workspace root is set correctly (Raven uses it for `source()` path resolution). Open the folder containing your R project, not a parent directory.
