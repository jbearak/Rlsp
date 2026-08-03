import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { expect, test } from "bun:test";

const repoRoot = path.resolve(__dirname, "..", "..");
const checkerPath = path.join(
  repoRoot,
  "editors",
  "vscode",
  "scripts",
  "check-stan-diagnostics-fixtures.mjs",
);

function sha256(content: string | Buffer): string {
  return createHash("sha256").update(content).digest("hex");
}

function createExternalFixture(): {
  directory: string;
  manifestPath: string;
  indexPath: string;
} {
  const directory = mkdtempSync(path.join(tmpdir(), "raven-stan-oracle-"));
  const materialized = path.join(directory, "materialized");
  const cases = path.join(materialized, "cases", "example");
  mkdirSync(cases, { recursive: true });

  const manifest = {
    schema_version: 1,
    language: "stan",
    sources: [{
      id: "example",
      discovery: [
        { kind: "complete", raven_mode: "all", oracle_mode: "stanc" },
        {
          kind: "documentation-snippet",
          raven_mode: "oracle-classified",
          oracle_mode: "stanc-contextual",
        },
      ],
    }],
  };
  const manifestPath = path.join(directory, "stan.json");
  const manifestText = JSON.stringify(manifest);
  writeFileSync(manifestPath, manifestText);

  const direct = "parameters { real x; } model { x ~ normal(0, 1); }\n";
  const contextual = "1 + 2";
  writeFileSync(path.join(cases, "direct.stan"), direct);
  writeFileSync(path.join(cases, "contextual.stan"), contextual);
  const index = {
    schema_version: 1,
    manifest_binding: [{ path: "stan.json", sha256: sha256(manifestText) }],
    cases: [
      {
        id: "example:direct",
        language: "stan",
        source_id: "example",
        materialized_path: "cases/example/direct.stan",
        sha256: sha256(direct),
        kind: "complete",
        raven_mode: "all",
        oracle_mode: "stanc",
      },
      {
        id: "example:contextual",
        language: "stan",
        source_id: "example",
        materialized_path: "cases/example/contextual.stan",
        sha256: sha256(contextual),
        kind: "documentation-snippet",
        raven_mode: "oracle-classified",
        oracle_mode: "stanc-contextual",
      },
    ],
    counts: { total: 2, stan: 2, jags: 0 },
  };
  const indexPath = path.join(materialized, "index.json");
  writeFileSync(indexPath, JSON.stringify(index));
  return { directory, manifestPath, indexPath };
}

function runChecker(directory: string, manifestPath: string) {
  return spawnSync(
    "node",
    [
      checkerPath,
      "--check-external",
      "--external-root",
      directory,
      "--external-manifest",
      manifestPath,
    ],
    { cwd: repoRoot, encoding: "utf8" },
  );
}

test("Stan external checker verifies direct and wrapped materialized cases", () => {
  const fixture = createExternalFixture();
  try {
    const result = runChecker(fixture.directory, fixture.manifestPath);
    expect(result.status, `${result.stderr}\n${result.stdout}`).toBe(0);
    const report = JSON.parse(
      readFileSync(
        path.join(fixture.directory, "materialized", "stan-oracle.json"),
        "utf8",
      ),
    );
    expect(report.counts).toEqual({
      total: 2,
      accepted_direct: 1,
      accepted_wrapped: 1,
      rejected: 0,
      verified: 2,
    });
    expect(report.verified_cases[0].raven_mode).toBe("all");
    expect(report.verified_cases[1].raven_mode).toBe("syntax-only");
    expect(report.verified_cases[1].materialized_path).toStartWith(
      "materialized/oracle-cases/stan/",
    );
  } finally {
    rmSync(fixture.directory, { recursive: true, force: true });
  }
});

test("Stan external checker rejects semantic failures declared for full diagnostics", () => {
  const fixture = createExternalFixture();
  try {
    const invalid = "model { missing_target ~ normal(0, 1); }\n";
    writeFileSync(
      path.join(fixture.directory, "materialized", "cases", "example", "direct.stan"),
      invalid,
    );
    const index = JSON.parse(readFileSync(fixture.indexPath, "utf8"));
    index.cases[0].sha256 = sha256(invalid);
    writeFileSync(fixture.indexPath, JSON.stringify(index));

    const result = runChecker(fixture.directory, fixture.manifestPath);
    expect(result.status).not.toBe(0);
    expect(`${result.stderr}${result.stdout}`).toContain(
      "raven_mode=all but stanc3 rejected the complete model",
    );
  } finally {
    rmSync(fixture.directory, { recursive: true, force: true });
  }
});

test("Stan external checker rejects materialized hash drift", () => {
  const fixture = createExternalFixture();
  try {
    const index = JSON.parse(readFileSync(fixture.indexPath, "utf8"));
    index.cases[0].sha256 = "0".repeat(64);
    writeFileSync(fixture.indexPath, JSON.stringify(index));
    const result = runChecker(fixture.directory, fixture.manifestPath);
    expect(result.status).not.toBe(0);
    expect(`${result.stderr}${result.stdout}`).toContain("SHA-256 mismatch");
  } finally {
    rmSync(fixture.directory, { recursive: true, force: true });
  }
});
