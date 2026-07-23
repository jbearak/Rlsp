import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { stanc, stanc_version } from "stanc3";

const require = createRequire(import.meta.url);
const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "../../..");
const fixtureRoot = path.join(repoRoot, "crates/raven/tests/fixtures/stan");
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

const valid = readGroup("valid");
const generated = readGroup("generated");
const syntaxOnly = readGroup("syntax_only");
const invalid = readGroup("invalid");
const includes = readGroup("includes");

if (valid.length !== 58 || syntaxOnly.length !== 18 || invalid.length !== 41) {
  throw new Error(
    `Unexpected curated fixture counts: valid=${valid.length}, syntax_only=${syntaxOnly.length}, invalid=${invalid.length}`,
  );
}
if (JSON.stringify(generated) !== JSON.stringify(expectedGenerated())) {
  throw new Error(
    "generated.json drifted; regenerate it from generatedModel() with its fixed 0..127 seed",
  );
}

for (const entry of [...valid, ...generated, ...includes]) {
  const result = compile(entry);
  if (result.errors !== undefined) {
    throw new Error(`${entry.name} must compile:\n${result.errors.join("\n")}`);
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
  `Stan fixture oracle passed: ${valid.length + generated.length + includes.length} valid, ${syntaxOnly.length} semantic/type-invalid, ${invalid.length} syntax-invalid`,
);
