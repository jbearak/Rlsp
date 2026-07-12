#!/usr/bin/env bun
// Regenerates `rmd-language-configuration.json` from the R language
// configuration, omitting the assignment onEnterRule that would also fire in
// Markdown prose.
//
// Usage:
//   bun editors/vscode/scripts/generate-rmd-language-config.mjs

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const vscodeRoot = resolve(here, "..");
const inputPath = resolve(vscodeRoot, "language-configuration.json");
const outputPath = resolve(vscodeRoot, "rmd-language-configuration.json");

const config = JSON.parse(readFileSync(inputPath, "utf8"));
const rules = config.onEnterRules ?? [];
const filteredRules = rules.filter(
  (rule) => !rule.beforeText?.includes("<<-"),
);
if (filteredRules.length !== rules.length - 1) {
  throw new Error("expected exactly one assignment onEnterRule containing <<-");
}
config.onEnterRules = filteredRules;

writeFileSync(outputPath, JSON.stringify(config, null, 2) + "\n");
