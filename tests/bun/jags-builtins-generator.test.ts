import { spawnSync } from "node:child_process";
import {
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { expect, test } from "bun:test";

const repoRoot = path.resolve(__dirname, "..", "..");
const generatorPath = path.join(
  repoRoot,
  "editors",
  "vscode",
  "scripts",
  "generate-jags-builtins.mjs",
);
const sourceManifestPath = path.join(
  repoRoot,
  "editors",
  "vscode",
  "scripts",
  "jags-builtins-4.3.2.tsv",
);
const sourceManifest = readFileSync(sourceManifestPath, "utf8");

type ManifestFields = [string, string, string, string, string];

function mutateEntry(
  source: string,
  kind: string,
  name: string,
  mutate: (fields: ManifestFields) => void,
): string {
  const lines = source.split(/\r?\n/);
  const index = lines.findIndex((line) => {
    const fields = line.split("\t");
    return fields[0] === kind && fields[2] === name;
  });
  if (index < 0) throw new Error(`Missing ${kind} manifest entry ${name}`);

  const fields = lines[index].split("\t") as ManifestFields;
  if (fields.length !== 5) throw new Error(`Malformed source entry ${kind}:${name}`);
  mutate(fields);
  lines[index] = fields.join("\t");
  return lines.join("\n");
}

function runGenerator(manifest: string): {
  status: number | null;
  output: string;
} {
  const directory = mkdtempSync(path.join(tmpdir(), "raven-jags-generator-"));
  const manifestPath = path.join(directory, "manifest.tsv");
  writeFileSync(manifestPath, manifest);
  try {
    const result = spawnSync("bun", [generatorPath, "--check"], {
      cwd: repoRoot,
      encoding: "utf8",
      env: {
        ...process.env,
        RAVEN_JAGS_BUILTINS_MANIFEST: manifestPath,
      },
    });
    return {
      status: result.status,
      output: `${result.stderr ?? ""}${result.stdout ?? ""}`,
    };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

function expectRejected(manifest: string, message: RegExp): void {
  const result = runGenerator(manifest);
  expect(result.status).not.toBe(0);
  expect(result.output).toMatch(message);
}

test("JAGS generator rejects a blank arity", () => {
  const manifest = mutateEntry(sourceManifest, "callable", "abs", (fields) => {
    fields[4] = "";
  });
  expectRejected(manifest, /Invalid arity ""/);
});

for (const arity of ["1e2", "0x2"]) {
  test(`JAGS generator rejects non-decimal arity ${arity}`, () => {
    const manifest = mutateEntry(sourceManifest, "callable", "abs", (fields) => {
      fields[4] = arity;
    });
    expectRejected(manifest, new RegExp(`Invalid arity "${arity}"`));
  });
}

test("JAGS generator rejects a dangling same-role canonical target", () => {
  const manifest = mutateEntry(sourceManifest, "callable", "acos", (fields) => {
    fields[3] = "missing.canonical";
  });
  expectRejected(manifest, /does not resolve to a same-role entry/);
});

test("JAGS generator rejects alias arity drift", () => {
  const manifest = mutateEntry(sourceManifest, "callable", "acos", (fields) => {
    fields[4] = "2";
  });
  expectRejected(manifest, /Alias acos arity 2 does not match canonical arccos arity 1/);
});

test("JAGS generator rejects alias module drift", () => {
  const manifest = mutateEntry(sourceManifest, "callable", "acos", (fields) => {
    fields[1] = "basemod";
  });
  expectRejected(
    manifest,
    /Alias acos module basemod does not match canonical arccos module bugs/,
  );
});

test("JAGS generator rejects an invalid callable module", () => {
  const manifest = mutateEntry(sourceManifest, "callable", "abs", (fields) => {
    fields[1] = "basemod";
  });
  expectRejected(manifest, /Invalid kind\/module pair callable\/basemod for abs/);
});

test("JAGS generator accepts the documented pow-to-operator exception", () => {
  expect(sourceManifest).toContain("callable\tbasemod\tpow\t^\t2");
  const result = runGenerator(sourceManifest);
  expect(result.status, result.output).toBe(0);
});
