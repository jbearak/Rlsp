import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import {
  lstatSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  renameSync,
  statSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { stanc, stanc_version } from "stanc3";

const require = createRequire(import.meta.url);
const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "../../..");
const fixtureRoot = path.join(repoRoot, "crates/raven/tests/fixtures/stan");
const defaultExternalManifest = path.join(
  repoRoot,
  "crates/raven/tests/fixtures/diagnostic_corpora/stan.json",
);
const packageVersion = JSON.parse(
  readFileSync(
    path.join(path.dirname(require.resolve("stanc3")), "../package.json"),
    "utf8",
  ),
).version;

if (packageVersion !== "2.39.1" || stanc_version !== "stanc3 v2.39.0") {
  throw new Error(
    `Fixture oracle requires stanc3 npm 2.39.1 / compiler 2.39.0; found ${packageVersion} / ${stanc_version}`,
  );
}

function readGroup(name) {
  return JSON.parse(readFileSync(path.join(fixtureRoot, `${name}.json`), "utf8"));
}

function generatedModel(i) {
  const prior = i & 1 ? "theta ~ student_t(4, 0, 1);" : "theta ~ normal(0, 1);";
  const likelihood = i & 2
    ? "for (n in 1:N) y[n] ~ normal(theta, sigma);"
    : "y ~ normal(theta, sigma);";
  const transformedData = i & 4
    ? "transformed data { real y_bar = mean(y); }\n"
    : "";
  const transformedParameters = i & 8
    ? "transformed parameters { real shifted = theta + 1; }\n"
    : "";
  const conditional = i & 16
    ? "if (theta > 0) target += -0.01 * theta;\n  "
    : "";
  const generatedExpression = i & 32 ? "normal_rng(theta, sigma)" : "theta + sigma";
  const functions = i & 64
    ? "functions { real sq(real x) { return x * x; } }\n"
    : "";
  const scaleExpression = i & 64 ? "sq(sigma)" : "sigma";
  return `${functions}data { int<lower=1> N; vector[N] y; }\n${transformedData}parameters { real theta; real<lower=0> sigma; }\n${transformedParameters}model {\n  ${prior}\n  sigma ~ exponential(1);\n  ${conditional}${likelihood}\n  target += -0.0001 * ${scaleExpression};\n}\ngenerated quantities { real draw = ${generatedExpression}; }\n`;
}

function expectedGenerated() {
  return Array.from({ length: 128 }, (_, i) => ({
    name: `pairwise-${String(i).padStart(3, "0")}`,
    code: generatedModel(i),
  }));
}

function compile(entry) {
  return stanc(`${entry.name}.stan`, entry.code, [], entry.includes);
}

const stanIdentifierCharacter = /[A-Za-z0-9_]/;

function containsIdentifier(message, identifier) {
  if (identifier.length === 0) {
    return false;
  }
  let offset = 0;
  while (offset <= message.length - identifier.length) {
    const index = message.indexOf(identifier, offset);
    if (index === -1) {
      return false;
    }
    const before = message[index - 1];
    const after = message[index + identifier.length];
    if (
      (before === undefined || !stanIdentifierCharacter.test(before))
      && (after === undefined || !stanIdentifierCharacter.test(after))
    ) {
      return true;
    }
    offset = index + 1;
  }
  return false;
}

for (const [message, identifier, expected] of [
  ["n is not defined", "n", true],
  ["unknown_value is not defined", "unknown_value", true],
  ["unknown_value_2 is not defined", "unknown_value", false],
  ["unrelated diagnostic", "n", false],
]) {
  if (containsIdentifier(message, identifier) !== expected) {
    throw new Error(`Identifier-boundary matcher failed for ${identifier}: ${message}`);
  }
}

function checkCuratedFixtures() {
  const valid = readGroup("valid");
  const generated = readGroup("generated");
  const syntaxOnly = readGroup("syntax_only");
  const semanticScope = readGroup("semantic_scope");
  const semanticScopeValid = readGroup("semantic_scope_valid");
  const invalid = readGroup("invalid");
  const includes = readGroup("includes");

  // Single-defect name-resolution cases consumed by Raven's native semantic
  // integration test. The stanc3 oracle must continue to classify these as
  // semantic (not syntax) failures and name the same missing identifier.
  const undeclaredVariableCases = new Map([
    ["unknown-variable", "unknown_value"],
    ["invalid-bound-reference", "missing_bound"],
  ]);

  if (
    valid.length !== 58
    || syntaxOnly.length !== 18
    || semanticScope.length !== 9
    || semanticScopeValid.length !== 13
    || invalid.length !== 41
  ) {
    throw new Error(
      `Unexpected curated fixture counts: valid=${valid.length}, syntax_only=${syntaxOnly.length}, semantic_scope=${semanticScope.length}, semantic_scope_valid=${semanticScopeValid.length}, invalid=${invalid.length}`,
    );
  }

  if (JSON.stringify(generated) !== JSON.stringify(expectedGenerated())) {
    throw new Error(
      "generated.json drifted; regenerate it from generatedModel() with its fixed 0..127 seed",
    );
  }

  for (const entry of [...valid, ...generated, ...includes, ...semanticScopeValid]) {
    const result = compile(entry);
    if (result.errors !== undefined) {
      throw new Error(`${entry.name} must compile:\n${result.errors.join("\n")}`);
    }
  }

  for (const entry of semanticScope) {
    const result = compile(entry);
    if (result.errors === undefined) {
      throw new Error(`${entry.name} must fail semantic name resolution`);
    }
    if (result.errors.some((message) => message.includes("Syntax error"))) {
      throw new Error(`${entry.name} is not syntax-valid:\n${result.errors.join("\n")}`);
    }
    if (!result.errors.some((message) => containsIdentifier(message, entry.missing))) {
      throw new Error(
        `${entry.name} no longer identifies ${entry.missing}:\n${result.errors.join("\n")}`,
      );
    }
  }

  for (const entry of syntaxOnly) {
    const result = compile(entry);
    if (result.errors === undefined) {
      throw new Error(`${entry.name} must fail semantic/type checking`);
    }
    if (result.errors.some((message) => message.includes("Syntax error"))) {
      throw new Error(`${entry.name} is not syntax-only:\n${result.errors.join("\n")}`);
    }
    const expectedMissingName = undeclaredVariableCases.get(entry.name);
    if (
      expectedMissingName !== undefined
      && !result.errors.some((message) => containsIdentifier(message, expectedMissingName))
    ) {
      throw new Error(
        `${entry.name} no longer identifies ${expectedMissingName}:\n${result.errors.join("\n")}`,
      );
    }
  }

  for (const name of undeclaredVariableCases.keys()) {
    if (!syntaxOnly.some((entry) => entry.name === name)) {
      throw new Error(`Missing semantic oracle fixture ${name}`);
    }
  }

  for (const entry of invalid) {
    const result = compile(entry);
    if (result.errors === undefined) {
      throw new Error(`${entry.name} must fail syntax checking`);
    }
    if (!result.errors.some((message) => message.includes("Syntax error"))) {
      throw new Error(`${entry.name} did not produce a syntax error:\n${result.errors.join("\n")}`);
    }
  }

  console.log(
    `Stan fixture oracle passed: ${valid.length + generated.length + includes.length + semanticScopeValid.length} valid, ${syntaxOnly.length + semanticScope.length} semantic/type-invalid, ${invalid.length} syntax-invalid`,
  );
}

function parseArguments(argv) {
  if (argv.length === 0) {
    return { checkExternal: false };
  }
  let checkExternal = false;
  let externalRoot;
  let externalManifest = defaultExternalManifest;
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--check-external") {
      checkExternal = true;
    } else if (argument === "--external-root" || argument === "--external-manifest") {
      const value = argv[index + 1];
      if (value === undefined) {
        throw new Error(`${argument} requires a path`);
      }
      index += 1;
      if (argument === "--external-root") externalRoot = path.resolve(value);
      else externalManifest = path.resolve(value);
    } else {
      throw new Error(`Unknown argument: ${argument}`);
    }
  }
  if (!checkExternal) {
    throw new Error("--external-root/--external-manifest require --check-external");
  }
  if (externalRoot === undefined) {
    throw new Error("--check-external requires --external-root");
  }
  return { checkExternal, externalRoot, externalManifest };
}

function sha256Bytes(content) {
  return createHash("sha256").update(content).digest("hex");
}

function readJson(file, description) {
  try {
    return JSON.parse(readFileSync(file, "utf8"));
  } catch (error) {
    throw new Error(`Cannot read ${description} ${file}: ${error.message}`);
  }
}

function requireNonEmptyString(record, key, context) {
  const value = record?.[key];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${context}.${key} must be a non-empty string`);
  }
  return value;
}

function safeMaterializedFile(materializedRoot, relativePath, context) {
  if (
    typeof relativePath !== "string"
    || relativePath.length === 0
    || relativePath.includes("\\")
    || path.posix.isAbsolute(relativePath)
  ) {
    throw new Error(`${context}.materialized_path must be a relative POSIX path`);
  }
  const parts = relativePath.split("/");
  if (parts.some((part) => part.length === 0 || part === "." || part === "..")) {
    throw new Error(`${context}.materialized_path contains an unsafe component`);
  }
  const candidate = path.join(materializedRoot, ...parts);
  let current = materializedRoot;
  for (const part of parts) {
    current = path.join(current, part);
    if (lstatSync(current).isSymbolicLink()) {
      throw new Error(`${context}.materialized_path traverses a symbolic link`);
    }
  }
  if (!statSync(candidate).isFile()) {
    throw new Error(`${context}.materialized_path is not a regular file`);
  }
  const realRoot = realpathSync(materializedRoot);
  const realCandidate = realpathSync(candidate);
  if (realCandidate !== realRoot && !realCandidate.startsWith(`${realRoot}${path.sep}`)) {
    throw new Error(`${context}.materialized_path escapes the materialized root`);
  }
  return candidate;
}

function loadExternalCases(manifestPath, externalRoot) {
  const manifest = readJson(manifestPath, "external manifest");
  if (manifest.schema_version !== 1 || manifest.language !== "stan") {
    throw new Error("External Stan manifest must have schema_version 1 and language stan");
  }
  if (!Array.isArray(manifest.sources) || manifest.sources.length === 0) {
    throw new Error("External Stan manifest sources must be a non-empty array");
  }

  const sourceRules = new Map();
  for (const [sourceIndex, source] of manifest.sources.entries()) {
    const context = `manifest.sources[${sourceIndex}]`;
    const sourceId = requireNonEmptyString(source, "id", context);
    if (sourceRules.has(sourceId)) throw new Error(`Duplicate external source id ${sourceId}`);
    if (!Array.isArray(source.discovery) || source.discovery.length === 0) {
      throw new Error(`${context}.discovery must be a non-empty array`);
    }
    const rules = new Set();
    for (const [discoveryIndex, discovery] of source.discovery.entries()) {
      const discoveryContext = `${context}.discovery[${discoveryIndex}]`;
      const oracleMode = requireNonEmptyString(discovery, "oracle_mode", discoveryContext);
      if (oracleMode !== "stanc" && oracleMode !== "stanc-contextual") {
        throw new Error(`${discoveryContext}.oracle_mode is unsupported: ${oracleMode}`);
      }
      rules.add(JSON.stringify([
        requireNonEmptyString(discovery, "kind", discoveryContext),
        requireNonEmptyString(discovery, "raven_mode", discoveryContext),
        oracleMode,
      ]));
    }
    sourceRules.set(sourceId, rules);
  }

  const materializedRoot = path.join(externalRoot, "materialized");
  const indexPath = path.join(materializedRoot, "index.json");
  const index = readJson(indexPath, "materialized index");
  if (index.schema_version !== 1 || !Array.isArray(index.cases)) {
    throw new Error("Materialized index must have schema_version 1 and a cases array");
  }
  const manifestDigest = sha256Bytes(readFileSync(manifestPath));
  if (
    !Array.isArray(index.manifest_binding)
    || !index.manifest_binding.some((binding) => binding?.sha256 === manifestDigest)
  ) {
    throw new Error("Materialized index is not bound to the supplied external manifest");
  }

  const seenIds = new Set();
  const seenPaths = new Map();
  const cases = [];
  for (const [caseIndex, record] of index.cases.entries()) {
    if (record?.language !== "stan") continue;
    const context = `index.cases[${caseIndex}]`;
    const id = requireNonEmptyString(record, "id", context);
    const relativePath = requireNonEmptyString(record, "materialized_path", context);
    if (seenIds.has(id)) throw new Error(`Duplicate materialized Stan case id ${id}`);
    const expectedHash = requireNonEmptyString(record, "sha256", context);
    const previousHash = seenPaths.get(relativePath);
    if (previousHash !== undefined && previousHash !== expectedHash) {
      throw new Error(`Conflicting materialized Stan path ${relativePath}`);
    }
    seenIds.add(id);
    seenPaths.set(relativePath, expectedHash);
    const sourceId = requireNonEmptyString(record, "source_id", context);
    const rule = JSON.stringify([
      requireNonEmptyString(record, "kind", context),
      requireNonEmptyString(record, "raven_mode", context),
      requireNonEmptyString(record, "oracle_mode", context),
    ]);
    if (!sourceRules.get(sourceId)?.has(rule)) {
      throw new Error(`${context} does not match a discovery rule for ${sourceId}`);
    }
    const sourcePath = safeMaterializedFile(materializedRoot, relativePath, context);
    const sourceBytes = readFileSync(sourcePath);
    const actualHash = sha256Bytes(sourceBytes);
    if (expectedHash !== actualHash) {
      throw new Error(`${id}: materialized SHA-256 mismatch (expected ${expectedHash}, got ${actualHash})`);
    }
    cases.push({ ...record, sourceBytes });
  }
  if (cases.length === 0) throw new Error("Materialized index contains no Stan cases");
  if (Number.isInteger(index.counts?.stan) && index.counts.stan !== cases.length) {
    throw new Error(`Materialized Stan count drifted: index=${index.counts.stan}, observed=${cases.length}`);
  }
  return { cases, indexPath, manifestDigest };
}

const contextualWrappers = [
  ["model-block", (source) => `model {\n${source}\n}\n`],
  ["functions-block", (source) => `functions {\n${source}\n}\n`],
  ["data-block", (source) => `data {\n${source}\n}\n`],
  ["transformed-data-block", (source) => `transformed data {\n${source}\n}\n`],
  ["parameters-block", (source) => `parameters {\n${source}\n}\n`],
  ["transformed-parameters-block", (source) => `transformed parameters {\n${source}\n}\n`],
  ["generated-quantities-block", (source) => `generated quantities {\n${source}\n}\n`],
  [
    "expression-in-transformed-data",
    (source) => `transformed data { real raven_external_probe = (${source}\n); }\n`,
  ],
];

function stancSyntaxOutcome(name, code) {
  const result = stanc(name, code, [], undefined);
  const errors = result.errors ?? [];
  return {
    syntaxAccepted: !errors.some((message) => message.includes("Syntax error")),
    semanticAccepted: result.errors === undefined,
  };
}

function verifyExternalCase(entry) {
  const source = entry.sourceBytes.toString("utf8");
  const attempts = [["direct", source]];
  if (entry.oracle_mode === "stanc-contextual") {
    attempts.push(...contextualWrappers.map(([name, wrap]) => [name, wrap(source)]));
  }
  for (const [wrapperId, code] of attempts) {
    const outcome = stancSyntaxOutcome(`${entry.id}.stan`, code);
    if (outcome.syntaxAccepted) {
      return {
        id: entry.id,
        source_id: entry.source_id,
        materialized_path: entry.materialized_path,
        sha256: entry.sha256,
        kind: entry.kind,
        raven_mode: entry.raven_mode,
        oracle_mode: entry.oracle_mode,
        outcome: wrapperId === "direct" ? "accepted-direct" : "accepted-wrapped",
        wrapper_id: wrapperId,
        syntax_accepted: true,
        semantic_accepted: outcome.semanticAccepted,
        verifiedSource: code,
      };
    }
  }
  return {
    id: entry.id,
    source_id: entry.source_id,
    materialized_path: entry.materialized_path,
    sha256: entry.sha256,
    kind: entry.kind,
    raven_mode: entry.raven_mode,
    oracle_mode: entry.oracle_mode,
    outcome: "rejected",
    wrapper_id: null,
    syntax_accepted: false,
    semantic_accepted: false,
  };
}

function checkExternalFixtures(manifestPath, externalRoot) {
  const { cases, indexPath, manifestDigest } = loadExternalCases(manifestPath, externalRoot);
  const results = cases.map(verifyExternalCase);
  const failures = results
    .filter((result) => result.raven_mode === "all" && !result.semantic_accepted)
    .map((result) => `${result.id}: raven_mode=all but stanc3 rejected the complete model`);
  const oracleCasesRoot = path.join(externalRoot, "materialized", "oracle-cases", "stan");
  mkdirSync(oracleCasesRoot, { recursive: true });
  const verifiedCases = results
    .filter((result) => (
      result.syntax_accepted
      && (result.raven_mode !== "all" || result.semantic_accepted)
    ))
    .map((result) => {
      const source = {
        materialized_path: `materialized/${result.materialized_path}`,
        sha256: result.sha256,
      };
      let materializedPath = `materialized/${result.materialized_path}`;
      let digest = result.sha256;
      if (result.outcome === "accepted-wrapped") {
        const bytes = Buffer.from(result.verifiedSource, "utf8");
        digest = sha256Bytes(bytes);
        const idDigest = sha256Bytes(Buffer.from(result.id, "utf8")).slice(0, 12);
        materializedPath = `materialized/oracle-cases/stan/${digest.slice(0, 16)}-${idDigest}.stan`;
        writeFileSync(
          path.join(externalRoot, ...materializedPath.split("/")),
          bytes,
        );
      }
      const ravenMode = result.raven_mode === "oracle-classified"
        ? (result.outcome === "accepted-direct" && result.semantic_accepted ? "all" : "syntax-only")
        : result.raven_mode;
      return {
        id: result.id,
        materialized_path: materializedPath,
        sha256: digest,
        raven_mode: ravenMode,
        wrapper_id: result.wrapper_id,
        source,
      };
    });
  for (const result of results) delete result.verifiedSource;
  const report = {
    schema_version: 1,
    oracle: {
      package_version: packageVersion,
      compiler_version: stanc_version,
    },
    inputs: {
      manifest_sha256: manifestDigest,
      materialized_index_sha256: sha256Bytes(readFileSync(indexPath)),
    },
    counts: {
      total: results.length,
      accepted_direct: results.filter((result) => result.outcome === "accepted-direct").length,
      accepted_wrapped: results.filter((result) => result.outcome === "accepted-wrapped").length,
      rejected: results.filter((result) => result.outcome === "rejected").length,
      verified: verifiedCases.length,
    },
    verified_cases: verifiedCases,
    outcomes: results,
    failures,
  };
  const reportPath = path.join(externalRoot, "materialized", "stan-oracle.json");
  const temporaryPath = `${reportPath}.tmp-${process.pid}`;
  writeFileSync(temporaryPath, `${JSON.stringify(report, null, 2)}\n`, "utf8");
  renameSync(temporaryPath, reportPath);
  if (failures.length > 0) {
    throw new Error(`External Stan oracle failed:\n${failures.join("\n")}`);
  }
  console.log(
    `External Stan oracle passed: ${report.counts.accepted_direct} direct, ${report.counts.accepted_wrapped} wrapped, ${report.counts.rejected} rejected/accounted`,
  );
}

const arguments_ = parseArguments(process.argv.slice(2));
if (arguments_.checkExternal) {
  checkExternalFixtures(arguments_.externalManifest, arguments_.externalRoot);
} else {
  checkCuratedFixtures();
}
