import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { expect, test } from "bun:test";

const repoRoot = path.resolve(__dirname, "..", "..");
const bundleScriptPath = path.join(
  repoRoot,
  "editors",
  "vscode",
  "scripts",
  "bundle-binary.js",
);
const binaryName = process.platform === "win32" ? "raven.exe" : "raven";
const alternateBinaryName = process.platform === "win32" ? "raven" : "raven.exe";

type BundleCase = {
  sourceContent?: string;
  destinationContent: string;
  sourceTime?: Date;
  destinationTime: Date;
  destinationName?: string;
};

function runBundleCase(testCase: BundleCase): {
  status: number | null;
  output: string;
  destinationContent: string;
  beforeMtimeMs: number;
  afterMtimeMs: number;
} {
  const root = mkdtempSync(path.join(tmpdir(), "raven-bundle-binary-"));
  const scriptDir = path.join(root, "editors", "vscode", "scripts");
  const binDir = path.join(root, "editors", "vscode", "bin");
  const releaseDir = path.join(root, "target", "release");
  const scriptPath = path.join(scriptDir, "bundle-binary.js");
  const sourcePath = path.join(releaseDir, binaryName);
  const destinationPath = path.join(
    binDir,
    testCase.destinationName ?? binaryName,
  );

  mkdirSync(scriptDir, { recursive: true });
  mkdirSync(binDir, { recursive: true });
  copyFileSync(bundleScriptPath, scriptPath);
  writeFileSync(destinationPath, testCase.destinationContent);
  utimesSync(destinationPath, testCase.destinationTime, testCase.destinationTime);
  const beforeMtimeMs = statSync(destinationPath).mtimeMs;

  if (testCase.sourceContent !== undefined) {
    mkdirSync(releaseDir, { recursive: true });
    writeFileSync(sourcePath, testCase.sourceContent);
    const sourceTime = testCase.sourceTime ?? testCase.destinationTime;
    utimesSync(sourcePath, sourceTime, sourceTime);
  }

  try {
    const result = spawnSync("bun", [scriptPath], {
      cwd: root,
      encoding: "utf8",
      env: {
        ...process.env,
        RAVEN_BUNDLE_NO_BUILD: "1",
      },
    });
    return {
      status: result.status,
      output: `${result.stdout ?? ""}${result.stderr ?? ""}`,
      destinationContent: readFileSync(destinationPath, "utf8"),
      beforeMtimeMs,
      afterMtimeMs: statSync(destinationPath).mtimeMs,
    };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("bundle-binary refreshes a destination older than the source", () => {
  const result = runBundleCase({
    sourceContent: "fresh",
    destinationContent: "stale",
    sourceTime: new Date("2021-01-01T00:00:00Z"),
    destinationTime: new Date("2020-01-01T00:00:00Z"),
  });

  expect(result.status, result.output).toBe(0);
  expect(result.destinationContent).toBe("fresh");
  expect(result.output).toContain("Refreshed stale raven binary");
});

test("bundle-binary leaves an up-to-date destination alone", () => {
  const result = runBundleCase({
    sourceContent: "current",
    destinationContent: "current",
    sourceTime: new Date("2020-01-01T00:00:00Z"),
    destinationTime: new Date("2021-01-01T00:00:00Z"),
  });

  expect(result.status, result.output).toBe(0);
  expect(result.destinationContent).toBe("current");
  expect(result.afterMtimeMs).toBe(result.beforeMtimeMs);
  expect(result.output).toContain("Bundled raven binary is already current");
});

test("bundle-binary refreshes a size mismatch when mtimes are equal", () => {
  const sameTime = new Date("2020-01-01T00:00:00Z");
  const result = runBundleCase({
    sourceContent: "fresh binary",
    destinationContent: "old",
    sourceTime: sameTime,
    destinationTime: sameTime,
  });

  expect(result.status, result.output).toBe(0);
  expect(result.destinationContent).toBe("fresh binary");
  expect(result.output).toContain("Refreshed stale raven binary");
});

test("bundle-binary preserves an alternate-platform binary when the source is missing", () => {
  const result = runBundleCase({
    destinationContent: "pre-placed",
    destinationTime: new Date("2020-01-01T00:00:00Z"),
    destinationName: alternateBinaryName,
  });

  expect(result.status, result.output).toBe(0);
  expect(result.destinationContent).toBe("pre-placed");
  expect(result.afterMtimeMs).toBe(result.beforeMtimeMs);
  expect(result.output).toContain(
    "Preserving pre-placed raven binary; no matching target/release binary found",
  );
});
