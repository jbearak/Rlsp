import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "../../..");
const manifestPath =
  process.env.RAVEN_JAGS_BUILTINS_MANIFEST ??
  path.join(scriptDir, "jags-builtins-4.3.2.tsv");
const outputPath = path.join(
  repoRoot,
  "crates",
  "raven",
  "src",
  "jags_builtins_generated.rs",
);

const EXPECTED_VERSION = "4.3.2";
const EXPECTED_SOURCE_URL =
  "https://downloads.sourceforge.net/project/mcmc-jags/JAGS/4.x/Source/JAGS-4.3.2.tar.gz";
const EXPECTED_SOURCE_SHA256 =
  "871f556af403a7c2ce6a0f02f15cf85a572763e093d26658ebac55c4ab472fc8";
const EXPECTED_MODULES = ["basemod", "bugs"];
const EXPECTED_KEYWORDS = ["data", "for", "in", "model", "var"];
const EXPECTED_CONTEXTUAL = ["I", "T"];
const EXPECTED_COMPILER_CALLABLES = new Set(["dim", "length"]);
const EXPECTED_BASEMOD_CALLABLES = new Set(["pow"]);

// JAGS registers `pow` as the spelling of the compiler's `^` operator rather
// than as an alias to another named callable. Keep external canonical targets
// closed to this one independently verified exception.
const EXTERNAL_CANONICAL_EXCEPTIONS = new Map([
  [
    "callable:pow",
    { canonicalName: "^", module: "basemod", arity: 2 },
  ],
]);

function compareAscii(left, right) {
  return left < right ? -1 : left > right ? 1 : 0;
}

function rustString(value) {
  return JSON.stringify(value);
}

function assertEqual(actual, expected, label) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(
      `${label} mismatch: expected ${JSON.stringify(expected)}, found ${JSON.stringify(actual)}`,
    );
  }
}

function validateAliases(entries) {
  const byRoleAndName = new Map(
    entries.map((entry) => [`${entry.kind}:${entry.name}`, entry]),
  );

  for (const entry of entries) {
    if (entry.canonicalName === entry.name) continue;

    const key = `${entry.kind}:${entry.name}`;
    const exception = EXTERNAL_CANONICAL_EXCEPTIONS.get(key);
    if (exception) {
      if (
        entry.canonicalName !== exception.canonicalName ||
        entry.module !== exception.module ||
        entry.arity !== exception.arity
      ) {
        throw new Error(
          `External canonical exception ${entry.name} must remain ` +
            `${exception.module}:${exception.canonicalName}/${exception.arity}`,
        );
      }
      continue;
    }

    const canonical = byRoleAndName.get(
      `${entry.kind}:${entry.canonicalName}`,
    );
    if (!canonical) {
      throw new Error(
        `Canonical target ${entry.canonicalName} for ${entry.kind} alias ` +
          `${entry.name} does not resolve to a same-role entry`,
      );
    }
    if (canonical.canonicalName !== canonical.name) {
      throw new Error(
        `Canonical target ${canonical.name} for ${entry.kind} alias ` +
          `${entry.name} is itself an alias`,
      );
    }
    if (entry.module !== canonical.module) {
      throw new Error(
        `Alias ${entry.name} module ${entry.module} does not match canonical ` +
          `${canonical.name} module ${canonical.module}`,
      );
    }
    if (entry.arity !== canonical.arity) {
      throw new Error(
        `Alias ${entry.name} arity ${entry.arity} does not match canonical ` +
          `${canonical.name} arity ${canonical.arity}`,
      );
    }
  }
}

function validateKindModule(entry) {
  if (entry.kind === "keyword" || entry.kind === "contextual") {
    if (entry.module !== "compiler") {
      throw new Error(
        `Invalid kind/module pair ${entry.kind}/${entry.module} for ${entry.name}`,
      );
    }
    return;
  }

  if (entry.kind === "distribution") {
    if (entry.module !== "bugs") {
      throw new Error(
        `Invalid kind/module pair distribution/${entry.module} for ${entry.name}`,
      );
    }
    return;
  }

  const expectedModule = EXPECTED_COMPILER_CALLABLES.has(entry.name)
    ? "compiler"
    : EXPECTED_BASEMOD_CALLABLES.has(entry.name)
      ? "basemod"
      : "bugs";
  if (entry.module !== expectedModule) {
    throw new Error(
      `Invalid kind/module pair callable/${entry.module} for ${entry.name}; ` +
        `expected ${expectedModule}`,
    );
  }
}

function parseManifest() {
  const raw = readFileSync(manifestPath, "utf8");
  const lines = raw.split(/\r?\n/);
  const version = /^# JAGS version: (.+)$/.exec(lines[1] ?? "")?.[1];
  const sourceUrl = /^# Source: (.+)$/.exec(lines[2] ?? "")?.[1];
  const sourceSha256 = /^# SHA-256: ([0-9a-f]{64})$/.exec(
    lines[3] ?? "",
  )?.[1];
  const modules = /^# Automatically loaded modules: (.+)$/.exec(
    lines[4] ?? "",
  )?.[1]
    ?.split(",")
    .map((value) => value.trim());

  if (version !== EXPECTED_VERSION) {
    throw new Error(`Expected JAGS ${EXPECTED_VERSION}, found ${version}`);
  }
  if (sourceUrl !== EXPECTED_SOURCE_URL) {
    throw new Error(`Unexpected JAGS source URL: ${sourceUrl}`);
  }
  if (sourceSha256 !== EXPECTED_SOURCE_SHA256) {
    throw new Error(`Unexpected JAGS source SHA-256: ${sourceSha256}`);
  }
  assertEqual(modules, EXPECTED_MODULES, "Automatically loaded modules");

  const entries = [];
  for (const [index, line] of lines.entries()) {
    if (!line || line.startsWith("#")) continue;
    const fields = line.split("\t");
    if (fields.length !== 5) {
      throw new Error(`Manifest line ${index + 1} must have five tab-separated fields`);
    }
    const [kind, module, name, canonicalName, rawArity] = fields;
    if (!new Set(["keyword", "contextual", "callable", "distribution"]).has(kind)) {
      throw new Error(`Unknown kind ${kind} on manifest line ${index + 1}`);
    }
    if (!new Set(["basemod", "bugs", "compiler"]).has(module)) {
      throw new Error(`Unknown module ${module} on manifest line ${index + 1}`);
    }
    if (!/^[A-Za-z][A-Za-z0-9._]*$/.test(name)) {
      throw new Error(`Invalid catalog name ${name} on manifest line ${index + 1}`);
    }
    if (!/^(?:[A-Za-z][A-Za-z0-9._]*|\^)$/.test(canonicalName)) {
      throw new Error(`Invalid canonical name ${canonicalName} on manifest line ${index + 1}`);
    }
    if (!/^(?:-1|0|[1-9][0-9]*)$/.test(rawArity)) {
      throw new Error(
        `Invalid arity ${JSON.stringify(rawArity)} on manifest line ${index + 1}; ` +
          "expected -1 or an unsigned base-10 integer",
      );
    }
    const arity = Number(rawArity);
    if (!Number.isSafeInteger(arity)) {
      throw new Error(
        `Invalid arity ${JSON.stringify(rawArity)} on manifest line ${index + 1}; ` +
          "value exceeds JavaScript's exact integer range",
      );
    }
    entries.push({ kind, module, name, canonicalName, arity });
  }

  for (const kind of ["keyword", "contextual", "callable", "distribution"]) {
    const group = entries.filter((entry) => entry.kind === kind);
    const names = group.map((entry) => entry.name);
    const sorted = [...names].sort(compareAscii);
    assertEqual(names, sorted, `${kind} entries must use ASCII sort order`);
    if (new Set(names).size !== names.length) {
      throw new Error(`${kind} entries must have unique names`);
    }
  }

  assertEqual(
    entries.filter((entry) => entry.kind === "keyword").map((entry) => entry.name),
    EXPECTED_KEYWORDS,
    "Syntax keywords",
  );
  assertEqual(
    entries
      .filter((entry) => entry.kind === "contextual")
      .map((entry) => entry.name),
    EXPECTED_CONTEXTUAL,
    "Contextual syntax",
  );
  validateAliases(entries);
  for (const entry of entries) validateKindModule(entry);

  return { version, sourceUrl, sourceSha256, entries };
}

function parameterLabels(entry) {
  if (entry.arity === -1) return ["value"];
  if (entry.arity === 0) return [];

  const exact = new Map([
    ["dim", ["variable"]],
    ["equals", ["left", "right"]],
    ["ifelse", ["condition", "yes", "no"]],
    ["inprod", ["left", "right"]],
    ["interp.lin", ["x", "x_values", "y_values"]],
    ["length", ["variable"]],
    ["pow", ["base", "exponent"]],
    ["rep", ["value", "times"]],
  ]);
  const labels = exact.get(entry.name);
  if (labels) return labels;

  const unary = new Set([
    "abs",
    "acos",
    "acosh",
    "arccos",
    "arccosh",
    "arcsin",
    "arcsinh",
    "arctan",
    "arctanh",
    "asin",
    "asinh",
    "atan",
    "atanh",
    "cloglog",
    "cos",
    "cosh",
    "exp",
    "icloglog",
    "ilogit",
    "inverse",
    "log",
    "logdet",
    "logfact",
    "loggam",
    "logit",
    "mean",
    "order",
    "phi",
    "probit",
    "rank",
    "round",
    "sd",
    "sin",
    "sinh",
    "sort",
    "sqrt",
    "step",
    "t",
    "tan",
    "tanh",
    "trunc",
  ]);
  if (entry.arity === 1 && unary.has(entry.name)) return ["x"];

  const first = entry.kind === "distribution" ? "parameter1" : "argument1";
  return Array.from({ length: entry.arity }, (_, index) =>
    index === 0
      ? first
      : `${entry.kind === "distribution" ? "parameter" : "argument"}${index + 1}`,
  );
}

function renderEntry(typeName, entry) {
  const labels = parameterLabels(entry);
  return [
    `    ${typeName} {`,
    `        name: ${rustString(entry.name)},`,
    `        canonical_name: ${rustString(entry.canonicalName)},`,
    `        module: ${rustString(entry.module)},`,
    `        parameters: &[${labels.map(rustString).join(", ")}],`,
    `        variadic: ${entry.arity === -1},`,
    "    },",
  ];
}

function renderGeneratedSource() {
  const { version, sourceUrl, sourceSha256, entries } = parseManifest();
  const keywords = entries.filter((entry) => entry.kind === "keyword");
  const contextual = entries.filter((entry) => entry.kind === "contextual");
  const callables = entries.filter((entry) => entry.kind === "callable");
  const distributions = entries.filter((entry) => entry.kind === "distribution");
  const lines = [
    "// @generated by `bun editors/vscode/scripts/generate-jags-builtins.mjs`.",
    "// Source facts: pinned JAGS registry manifest. DO NOT EDIT BY HAND.",
    "",
    `pub const JAGS_VERSION: &str = ${rustString(version)};`,
    `pub const JAGS_SOURCE_URL: &str = ${rustString(sourceUrl)};`,
    `pub const JAGS_SOURCE_SHA256: &str = ${rustString(sourceSha256)};`,
    "pub static JAGS_AUTOMATIC_MODULES: &[&str] = &[\"basemod\", \"bugs\"];",
    `pub static JAGS_KEYWORDS: &[&str] = &[${keywords.map((entry) => rustString(entry.name)).join(", ")}];`,
    `pub static JAGS_CONTEXTUAL_SYNTAX: &[&str] = &[${contextual.map((entry) => rustString(entry.name)).join(", ")}];`,
    "",
    "#[rustfmt::skip]",
    "pub static JAGS_CALLABLES: &[JagsCallable] = &[",
  ];
  for (const entry of callables) lines.push(...renderEntry("JagsCallable", entry));
  lines.push(
    "];",
    "",
    "#[rustfmt::skip]",
    "pub static JAGS_DISTRIBUTIONS: &[JagsDistribution] = &[",
  );
  for (const entry of distributions) {
    lines.push(...renderEntry("JagsDistribution", entry));
  }
  lines.push("];", "");
  return lines.join("\n");
}

const generated = renderGeneratedSource();
if (process.argv.includes("--check")) {
  const current = readFileSync(outputPath, "utf8");
  if (current !== generated) {
    throw new Error(
      "JAGS builtin catalog is stale. Run:\n  bun editors/vscode/scripts/generate-jags-builtins.mjs",
    );
  }
} else {
  writeFileSync(outputPath, generated);
}
