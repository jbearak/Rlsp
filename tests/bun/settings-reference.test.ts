import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";

import { test } from "bun:test";

const repoRoot = path.resolve(__dirname, "..", "..");
const generatorPath = path.join(
  repoRoot,
  "editors",
  "vscode",
  "scripts",
  "generate-settings-reference.mjs",
);
const packageJsonPath = path.join(repoRoot, "editors", "vscode", "package.json");
const tomlLoaderPath = path.join(
  repoRoot,
  "crates",
  "raven",
  "src",
  "config_file",
  "toml_loader.rs",
);

function settingsReferenceTomlPathOverrides(): Set<string> {
  const source = readFileSync(generatorPath, "utf8");
  const match = source.match(/const TOML_PATH_OVERRIDES = \{([\s\S]*?)\};/);
  if (!match) {
    throw new Error("Could not find TOML_PATH_OVERRIDES in settings-reference generator");
  }
  return new Set([...match[1].matchAll(/"([^"]+)":\s*null/g)].map((entry) => entry[1]));
}

function projectScopedLintingSchemaKeys(): string[] {
  const packageJson = JSON.parse(readFileSync(packageJsonPath, "utf8")) as {
    contributes?: { configuration?: { properties?: Record<string, unknown> } };
  };
  const properties = packageJson.contributes?.configuration?.properties ?? {};
  const noTomlPath = settingsReferenceTomlPathOverrides();
  return Object.keys(properties)
    .filter((key) => key.startsWith("raven.linting."))
    .filter((key) => !noTomlPath.has(key))
    .map((key) => key.replace(/^raven\.linting\./, ""))
    .sort();
}

function configurationProperties(): Record<string, Record<string, unknown>> {
  const packageJson = JSON.parse(readFileSync(packageJsonPath, "utf8")) as {
    contributes?: {
      configuration?: {
        properties?: Record<string, Record<string, unknown>>;
      };
    };
  };
  return packageJson.contributes?.configuration?.properties ?? {};
}

function rustKnownLintingKeys(): string[] {
  const source = readFileSync(tomlLoaderPath, "utf8");
  const match = source.match(/const KNOWN_LINTING_KEYS:[\s\S]*?= &\[([\s\S]*?)\];/);
  if (!match) {
    throw new Error("Could not find KNOWN_LINTING_KEYS in toml_loader.rs");
  }
  return [...match[1].matchAll(/"([^"]+)"/g)]
    .map((entry) => entry[1])
    .filter((key) => key !== "overrides")
    .sort();
}

test("docs/settings-reference.md matches the generator output", () => {
  const result = spawnSync("bun", [generatorPath, "--check"], {
    cwd: repoRoot,
    encoding: "utf8",
  });
  if (result.status !== 0) {
    const stderr = result.stderr ?? "";
    const stdout = result.stdout ?? "";
    throw new Error(
      `Settings reference is stale. Run:\n` +
        `  bun editors/vscode/scripts/generate-settings-reference.mjs\n\n` +
        `Generator stderr:\n${stderr}${stdout}`,
    );
  }
});

test("Quarto font schemas mirror Knit validation constraints", () => {
  const properties = configurationProperties();
  const validationKeys = [
    "type",
    "default",
    "scope",
    "maxLength",
    "pattern",
    "patternErrorMessage",
  ];

  for (const suffix of ["fontFamily", "monospaceFontFamily"]) {
    const quarto = properties[`raven.quarto.${suffix}`];
    const knit = properties[`raven.knit.${suffix}`];
    if (!quarto || !knit) {
      throw new Error(`Missing Quarto or Knit ${suffix} schema`);
    }
    for (const key of validationKeys) {
      if (quarto[key] !== knit[key]) {
        throw new Error(
          `raven.quarto.${suffix}.${key} must mirror raven.knit.${suffix}.${key}`,
        );
      }
    }
  }
});

test("settings reference renders indentationUnit allowed values", () => {
  const settingsReference = readFileSync(
    path.join(repoRoot, "docs", "settings-reference.md"),
    "utf8",
  );
  const row = settingsReference
    .split("\n")
    .find((line) => line.startsWith("| `raven.linting.indentationUnit` |"));
  if (!row) {
    throw new Error("Missing raven.linting.indentationUnit row");
  }
  if (!row.includes("integer (1–8)") || !row.includes('`"auto"`')) {
    throw new Error(
      `Expected indentationUnit row to document integer 1–8 and "auto"; got:\n${row}`,
    );
  }
});

test("settings reference explains dash raven.toml paths accurately", () => {
  const settingsReference = readFileSync(
    path.join(repoRoot, "docs", "settings-reference.md"),
    "utf8",
  );
  const expected =
    "A `—` means the setting is not read from `raven.toml`; most such settings are VS Code-client-only, and LSP-client-only exceptions say so in their descriptions.";
  if (!settingsReference.includes(expected)) {
    throw new Error(`Missing or stale raven.toml path legend:\n${expected}`);
  }
});

test("settings reference marks readHomeLintr as VS Code-only", () => {
  const settingsReference = readFileSync(
    path.join(repoRoot, "docs", "settings-reference.md"),
    "utf8",
  );
  const row = settingsReference
    .split("\n")
    .find((line) => line.startsWith("| `raven.linting.readHomeLintr` |"));
  if (!row) {
    throw new Error("Missing raven.linting.readHomeLintr row");
  }
  if (!row.includes("| `false` | boolean | — |")) {
    throw new Error(
      `Expected readHomeLintr row to have default false, boolean type, and no raven.toml path; got:\n${row}`,
    );
  }
  for (const expected of ["VS Code/LSP-client-only", "--config ~/.lintr"]) {
    if (!row.includes(expected)) {
      throw new Error(`Expected readHomeLintr row to mention ${expected}; got:\n${row}`);
    }
  }
});

test("settings reference keeps linting.enabled auto caveats", () => {
  const settingsReference = readFileSync(
    path.join(repoRoot, "docs", "settings-reference.md"),
    "utf8",
  );
  const row = settingsReference
    .split("\n")
    .find((line) => line.startsWith("| `raven.linting.enabled` |"));
  if (!row) {
    throw new Error("Missing raven.linting.enabled row");
  }
  for (const expected of ["~/.lintr", "readHomeLintr", "REditorSupport", "Positron"]) {
    if (!row.includes(expected)) {
      throw new Error(`Expected linting.enabled row to mention ${expected}; got:\n${row}`);
    }
  }
});

test("Rust raven.toml linting keys match VS Code project-scoped linting schema", () => {
  const rustKeys = rustKnownLintingKeys();
  const schemaKeys = projectScopedLintingSchemaKeys();
  if (JSON.stringify(rustKeys) !== JSON.stringify(schemaKeys)) {
    throw new Error(
      "Rust KNOWN_LINTING_KEYS must match project-scoped raven.linting.* schema keys.\n" +
        `Rust:   ${JSON.stringify(rustKeys)}\n` +
        `Schema: ${JSON.stringify(schemaKeys)}\n`,
    );
  }
});
