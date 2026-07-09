#!/usr/bin/env bun
// Regenerates `docs/settings-reference.md` from `editors/vscode/package.json`,
// the single source of truth for every `raven.*` setting the VS Code extension
// exposes. The generated table is a flat alphabetical index — for narrative
// explanations of each section, see the per-feature docs linked from each row.
//
// Usage:
//   bun editors/vscode/scripts/generate-settings-reference.mjs           # rewrite
//   bun editors/vscode/scripts/generate-settings-reference.mjs --check   # CI drift gate

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..", "..", "..");
const packageJsonPath = resolve(repoRoot, "editors/vscode/package.json");
const outputPath = resolve(repoRoot, "docs/settings-reference.md");

// Top-level TOML sections recognized by `raven.toml`. Mirror of
// `KNOWN_TOP_LEVEL` in `crates/raven/src/config_file/toml_loader.rs`.
const TOML_SECTIONS = new Set([
  "linting",
  "crossFile",
  "packages",
  "diagnostics",
  "indentation",
  "symbols",
  "completion",
]);

// Server-side knobs the language server reads from `initializationOptions` /
// `raven.toml` but the VS Code extension deliberately doesn't surface in the
// Settings UI (typically because they're advanced / rarely-tuned). They are
// still real `raven.*` keys, so the alphabetical index covers them — values
// here mirror the defaults baked into the corresponding Rust config struct.
//
// When you add or rename one of these, update this list AND the parser in
// `crates/raven/src/backend.rs` (`parse_cross_file_config`, etc.). The drift
// test in `tests/bun/settings-reference.test.ts` only catches `package.json` ↔
// docs drift, not Rust ↔ this list drift; keep these descriptions short and
// link out to the narrative doc for nuance.
const LSP_INIT_ONLY_SETTINGS = {
  "raven.crossFile.hoistGlobalsInFunctions": {
    type: "boolean",
    default: true,
    description:
      "Hoist global definitions inside function bodies so callers see late-binding semantics across files.",
  },
  "raven.crossFile.editedFileDebounceMs": {
    type: "number",
    default: 50,
    minimum: 0,
    description:
      "Debounce delay (ms) for re-running diagnostics on the actively-edited file.",
  },
};

// First path segment (after `raven.`) → feature doc. The narrative pages
// under `docs/` already cover each surface; this table just points at them.
const DOC_LINKS = {
  linting: "linting.md",
  crossFile: "cross-file.md",
  packages: "r-package-dev.md",
  diagnostics: "diagnostics.md",
  indentation: "indentation.md",
  symbols: "document-outline.md",
  completion: "completion.md",
  chunks: "chunks.md",
  dataViewer: "data-viewer.md",
  knit: "knit.md",
  pandoc: "knit.md",
  sendToR: "r-console.md",
  rTerminal: "r-console.md",
  rConsole: "r-console.md",
  editor: "editor-integrations.md",
  plot: "plot-viewer.md",
  help: "help-viewer.md",
  server: "configuration.md",
  trace: "configuration.md",
};

const DOC_LINK_OVERRIDES = {
  "raven.packages.rprofilePrelude": "rprofile.md",
};

const TOML_PATH_OVERRIDES = {
  "raven.linting.readHomeLintr": null,
  "raven.crossFile.caseMismatchSeverity": "crossFile.diagnostics.caseMismatch",
  "raven.crossFile.missingFileSeverity": "crossFile.diagnostics.missingFile",
  "raven.diagnostics.undefinedVariableSeverity": "diagnostics.severity.undefinedVariable",
};

const DESCRIPTION_OVERRIDES = {
  "raven.linting.enabled":
    '"auto" enables linting when a .lintr or raven.toml opts in, except literal ~/.lintr is ignored unless the LSP-client setting raven.linting.readHomeLintr is true or the CLI receives --config ~/.lintr, and .lintr is ignored when REditorSupport or Positron already owns lintr diagnostics.',
  "raven.linting.readHomeLintr":
    "VS Code/LSP-client-only. Read literal ~/.lintr during project-config discovery; the CLI uses literal ~/.lintr only when passed explicitly with --config ~/.lintr.",
};

function escapeCell(text) {
  // Escape backslashes first so the pipe-escape pass doesn't double up an
  // already-escaped sequence (`\|` → `\\|`). Then normalize newlines so a
  // multi-line description still fits in one table row.
  return text
    .replace(/\\/g, "\\\\")
    .replace(/\|/g, "\\|")
    .replace(/\r?\n+/g, " ");
}

function inlineCode(text) {
  return "`" + text.replace(/`/g, "​`") + "`";
}

function formatDefault(value) {
  if (value === undefined) return "—";
  if (value === "") return inlineCode('""');
  if (typeof value === "string") return inlineCode(JSON.stringify(value));
  if (Array.isArray(value)) {
    if (value.length === 0) return inlineCode("[]");
    return inlineCode(JSON.stringify(value));
  }
  return inlineCode(JSON.stringify(value));
}

function formatType(schema) {
  if (Array.isArray(schema.oneOf)) {
    return schema.oneOf.map((variant) => formatType(variant)).join(" \\| ");
  }
  if (schema.const !== undefined) {
    return inlineCode(JSON.stringify(schema.const));
  }
  if (Array.isArray(schema.enum)) {
    return schema.enum.map((v) => inlineCode(JSON.stringify(v))).join(" \\| ");
  }
  const type = schema.type ?? "any";
  if (type === "array") {
    const itemType = schema.items?.type ?? "any";
    if (Array.isArray(schema.items?.enum)) {
      const choices = schema.items.enum
        .map((v) => inlineCode(JSON.stringify(v)))
        .join(" \\| ");
      return `array of (${choices})`;
    }
    return `array of ${itemType}`;
  }
  if (type === "integer" || type === "number") {
    const min = schema.minimum;
    const max = schema.maximum;
    if (min !== undefined && max !== undefined) {
      return `${type} (${min}–${max})`;
    }
    if (min !== undefined) return `${type} (≥${min})`;
    if (max !== undefined) return `${type} (≤${max})`;
    return type;
  }
  return type;
}

function firstSentence(text) {
  // Strip leading whitespace, then take through the first period followed by
  // whitespace or end-of-string. Avoids cutting at `e.g.` / `i.e.` / decimals
  // by requiring the period be followed by whitespace+capital or end-of-string.
  const trimmed = text.trim();
  const match = trimmed.match(/^(.+?[.!?])(\s+[A-Z]|\s*$)/);
  if (!match) return trimmed;
  return match[1];
}

function tomlPath(key) {
  if (Object.prototype.hasOwnProperty.call(TOML_PATH_OVERRIDES, key)) {
    return TOML_PATH_OVERRIDES[key];
  }
  // `raven.foo.bar` → `foo` is the first segment.
  const parts = key.split(".");
  if (parts.length < 2 || parts[0] !== "raven") return null;
  const [, section, ...rest] = parts;
  if (!TOML_SECTIONS.has(section)) return null;
  return [section, ...rest].join(".");
}

function docLink(key) {
  const override = DOC_LINK_OVERRIDES[key];
  if (override) return `[${override.replace(/\.md$/, "")}](${override})`;
  const section = key.split(".")[1];
  const target = DOC_LINKS[section];
  if (!target) return "—";
  return `[${target.replace(/\.md$/, "")}](${target})`;
}

function buildRow(key, schema) {
  const description = schema.markdownDescription ?? schema.description ?? "";
  const oneLine = DESCRIPTION_OVERRIDES[key] ?? firstSentence(description);
  const tomlCell = tomlPath(key) ? inlineCode(tomlPath(key)) : "—";
  return [
    inlineCode(key),
    formatDefault(schema.default),
    formatType(schema),
    tomlCell,
    docLink(key),
    escapeCell(oneLine),
  ];
}

function buildMarkdown(properties) {
  const keys = Object.keys(properties).sort((a, b) => a.localeCompare(b));
  const header = [
    "Setting",
    "Default",
    "Type / Allowed values",
    "`raven.toml` path",
    "Docs",
    "Description",
  ];
  const align = ["---", "---", "---", "---", "---", "---"];
  const rows = keys.map((key) => buildRow(key, properties[key]));

  const lines = [];
  lines.push("<!--");
  lines.push("  Auto-generated by editors/vscode/scripts/generate-settings-reference.mjs.");
  lines.push("  Do not edit by hand — run `bun editors/vscode/scripts/generate-settings-reference.mjs`");
  lines.push("  after changing settings in editors/vscode/package.json. The drift test in");
  lines.push("  tests/bun/settings-reference.test.ts gates this on CI.");
  lines.push("-->");
  lines.push("");
  lines.push("# Settings reference");
  lines.push("");
  lines.push(
    "Every `raven.*` setting Raven reads, in one alphabetical table. " +
      "Most are exposed through the VS Code Settings UI; a handful of advanced " +
      "server-side knobs are LSP-init-only (still readable from `raven.toml`). " +
      "For narrative explanations of each surface, follow the **Docs** link on a row. " +
      "For how `raven.toml` layering works, see [Configuration](configuration.md)."
  );
  lines.push("");
  lines.push(
    "The **`raven.toml` path** column shows where to set a key in a project's `raven.toml`. " +
      "A `—` means the setting is not read from `raven.toml`; most such settings are VS Code-client-only, and LSP-client-only exceptions say so in their descriptions."
  );
  lines.push("");
  lines.push(`| ${header.join(" | ")} |`);
  lines.push(`| ${align.join(" | ")} |`);
  for (const row of rows) {
    lines.push(`| ${row.join(" | ")} |`);
  }
  lines.push("");
  return lines.join("\n");
}

function main() {
  const packageJson = JSON.parse(readFileSync(packageJsonPath, "utf8"));
  const schemaProperties = packageJson?.contributes?.configuration?.properties;
  if (!schemaProperties || typeof schemaProperties !== "object") {
    console.error("Could not find contributes.configuration.properties in package.json");
    process.exit(2);
  }
  // Drop deprecated keys: they remain in package.json (for Settings-UI styling
  // and a graceful transition) but should not clutter the user-facing index.
  const activeSchemaProperties = Object.fromEntries(
    Object.entries(schemaProperties).filter(
      ([, schema]) =>
        !schema?.deprecationMessage && !schema?.markdownDeprecationMessage,
    ),
  );
  // Merge in LSP-init-only entries that aren't surfaced through the VS Code
  // Settings UI. Schema-derived keys still win on collision so a future
  // promotion of one of these to the UI just removes the entry here.
  const properties = { ...LSP_INIT_ONLY_SETTINGS, ...activeSchemaProperties };
  const generated = buildMarkdown(properties);

  const args = new Set(process.argv.slice(2));
  if (args.has("--check")) {
    let current = "";
    try {
      current = readFileSync(outputPath, "utf8");
    } catch {
      console.error(`Missing ${outputPath} — run the generator without --check.`);
      process.exit(1);
    }
    if (current !== generated) {
      console.error(
        `docs/settings-reference.md is out of date.\n` +
          `Run: bun editors/vscode/scripts/generate-settings-reference.mjs`
      );
      process.exit(1);
    }
    return;
  }

  writeFileSync(outputPath, generated);
  console.log(`Wrote ${outputPath}`);
}

main();
