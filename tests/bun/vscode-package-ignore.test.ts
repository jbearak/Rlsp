import { readFileSync } from "node:fs";
import path from "node:path";

import { expect, test } from "bun:test";

const repoRoot = path.resolve(__dirname, "..", "..");
const vscodeRoot = path.join(repoRoot, "editors", "vscode");

function vscodeIgnoreEntries(): Set<string> {
  return new Set(
    readFileSync(path.join(vscodeRoot, ".vscodeignore"), "utf8")
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter((line) => line.length > 0 && !line.startsWith("#")),
  );
}

test("VSIX package keeps runtime assets and excludes development-only files", () => {
  const packageJson = JSON.parse(
    readFileSync(path.join(vscodeRoot, "package.json"), "utf8"),
  ) as { icon?: string };
  const ignore = vscodeIgnoreEntries();

  expect(packageJson.icon).toBe("icon.png");
  expect(ignore.has("icon.svg")).toBe(true);
  expect(ignore.has("icon.png")).toBe(false);
  expect(ignore.has("dist/knit/**")).toBe(true);
  expect(ignore.has("dist/quarto-theme-bridge/**")).toBe(false);
  expect(ignore.has("test-fixtures/**")).toBe(true);

  const buildScript = readFileSync(
    path.join(vscodeRoot, "scripts", "build.js"),
    "utf8",
  );
  expect(buildScript).toContain("copyQuartoThemeBridge()");
  expect(buildScript).toContain("path.join(dist, 'quarto-theme-bridge')");
});
