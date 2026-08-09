import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
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
  prePlaced?: boolean;
  /**
   * When set, also read back `bin/<hostBinaryName>` after the run. Use it to
   * assert whether the host copy happened, which matters when the pre-placed
   * artifact under test carries the OTHER platform's filename.
   */
  hostBinaryName?: string;
};

function runBundleCase(testCase: BundleCase): {
  status: number | null;
  output: string;
  destinationContent: string;
  hostBinaryContent: string | undefined;
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
        ...(testCase.prePlaced ? { RAVEN_BUNDLE_PREPLACED: "1" } : {}),
      },
    });
    const hostBinaryPath = testCase.hostBinaryName
      ? path.join(binDir, testCase.hostBinaryName)
      : undefined;
    return {
      status: result.status,
      output: `${result.stdout ?? ""}${result.stderr ?? ""}`,
      destinationContent: readFileSync(destinationPath, "utf8"),
      hostBinaryContent:
        hostBinaryPath && existsSync(hostBinaryPath)
          ? readFileSync(hostBinaryPath, "utf8")
          : undefined,
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

// Release packaging cross-compiles, unzips the TARGET binary into bin/, then
// runs this script. A runner that also has a host `target/release/raven` (warm
// cache, self-hosted runner, local `vsce package` after a dev build) must not
// have that host binary overwrite the cross-compiled one — the VSIX would ship
// a server that cannot execute on the platform it claims to target. Freshness
// cannot catch this: the host binary is legitimately newer.
test("bundle-binary preserves a pre-placed binary even when target/release is newer", () => {
  const result = runBundleCase({
    sourceContent: "host binary for the wrong platform",
    destinationContent: "cross-compiled target binary",
    sourceTime: new Date("2021-01-01T00:00:00Z"),
    destinationTime: new Date("2020-01-01T00:00:00Z"),
    prePlaced: true,
  });

  expect(result.status, result.output).toBe(0);
  expect(result.destinationContent).toBe("cross-compiled target binary");
  expect(result.afterMtimeMs).toBe(result.beforeMtimeMs);
  expect(result.output).toContain(
    "Preserving pre-placed raven binary (RAVEN_BUNDLE_PREPLACED=1)",
  );
});

// A foreign-named artifact left in bin/ (e.g. raven.exe after packaging a
// Windows target on Linux) must NOT suppress the host copy. `copy-binary` also
// runs from `pretest` and ordinary dev builds; skipping the copy there would
// report success while leaving the extension — which resolves bin/raven from
// process.platform — with no runnable server. The foreign file is untouched
// regardless, because the copy writes to this host's filename.
test("bundle-binary still creates the host binary alongside a foreign-target one", () => {
  const result = runBundleCase({
    sourceContent: "host binary",
    destinationContent: "foreign target binary",
    sourceTime: new Date("2021-01-01T00:00:00Z"),
    destinationTime: new Date("2020-01-01T00:00:00Z"),
    destinationName: alternateBinaryName,
    hostBinaryName: binaryName,
  });

  expect(result.status, result.output).toBe(0);
  // The foreign artifact survives untouched...
  expect(result.destinationContent).toBe("foreign target binary");
  expect(result.afterMtimeMs).toBe(result.beforeMtimeMs);
  // ...and the host binary is created, so pretest/dev runs have a server.
  expect(result.hostBinaryContent).toBe("host binary");
  expect(result.output).toContain("Bundled raven binary");
});

// Release packaging still preserves a foreign-named artifact when it is marked
// pre-placed — that is the env-var guard's job, and it must win over the copy.
test("bundle-binary preserves a foreign-target binary when marked pre-placed", () => {
  const result = runBundleCase({
    sourceContent: "host binary",
    destinationContent: "cross-compiled target binary",
    sourceTime: new Date("2021-01-01T00:00:00Z"),
    destinationTime: new Date("2020-01-01T00:00:00Z"),
    destinationName: alternateBinaryName,
    hostBinaryName: binaryName,
    prePlaced: true,
  });

  expect(result.status, result.output).toBe(0);
  expect(result.destinationContent).toBe("cross-compiled target binary");
  expect(result.afterMtimeMs).toBe(result.beforeMtimeMs);
  expect(result.hostBinaryContent).toBeUndefined();
  expect(result.output).toContain(
    "Preserving pre-placed raven binary (RAVEN_BUNDLE_PREPLACED=1)",
  );
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
