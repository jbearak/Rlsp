import { existsSync, readFileSync, statSync } from "node:fs";
import path from "node:path";

import { describe, expect, test } from "bun:test";

const repoRoot = path.resolve(__dirname, "..", "..");

function readRepoFile(...parts: string[]): string {
  return readFileSync(path.join(repoRoot, ...parts), "utf8");
}

describe("apt packaging release integration", () => {
  test("Debian packaging scripts are present and executable", () => {
    for (const script of ["build-deb.sh", "update-apt-repo.sh"]) {
      const scriptPath = path.join(repoRoot, "scripts", "debian", script);
      expect(existsSync(scriptPath)).toBe(true);
      expect(statSync(scriptPath).mode & 0o111).not.toBe(0);
    }
    expect(existsSync(path.join(repoRoot, "scripts", "debian", "lib.sh"))).toBe(true);
  });

  test("build-deb declares the Raven runtime package metadata", () => {
    const script = readRepoFile("scripts", "debian", "build-deb.sh");
    expect(script).toContain("Package: raven");
    expect(script).toContain("dpkg-shlibdeps");
    expect(script).toContain("sub(/^shlibs:Depends=/");
    expect(script).toContain('depends="ca-certificates, curl"');
    expect(script).toContain("Depends: ${depends}");
    expect(script).toContain("/usr/bin/raven");
    expect(script).toContain("/usr/share/doc/raven/copyright");
  });

  test("Debian scripts share version validation and avoid GPG passphrases in argv", () => {
    const helper = readRepoFile("scripts", "debian", "lib.sh");
    const buildScript = readRepoFile("scripts", "debian", "build-deb.sh");
    const updateScript = readRepoFile("scripts", "debian", "update-apt-repo.sh");

    expect(helper).toContain("validate_raven_debian_version");
    expect(buildScript).toContain(". \"${script_dir}/lib.sh\"");
    expect(updateScript).toContain(". \"${script_dir}/lib.sh\"");
    expect(updateScript).toContain("--passphrase-file");
    expect(updateScript).not.toContain('--passphrase "$APT_GPG_PASSPHRASE"');
  });

  test("release workflow can publish apt repository PRs behind an explicit gate", () => {
    const workflow = readRepoFile(".github", "workflows", "release-build.yml");
    const aptJob = workflow.slice(
      workflow.indexOf("  bump-apt:"),
      workflow.indexOf("  # ── Homebrew tap bump"),
    );

    expect(workflow).toContain("bump-apt:");
    expect(workflow).toContain("vars.ENABLE_APT_BUMP == 'true'");
    expect(aptJob).toContain("APT_REPO_TOKEN");
    expect(aptJob).toContain("APT_REPO_GPG_PRIVATE_KEY");
    expect(aptJob).toContain("APT_REPO_GPG_PASSPHRASE");
    expect(aptJob).toContain("repository: jbearak/apt-raven");
    expect(aptJob).toContain("scripts/debian/build-deb.sh");
    expect(aptJob).toContain("scripts/debian/update-apt-repo.sh");
    expect(aptJob).toContain("VERSION: ${{ steps.version.outputs.version }}");
    expect(aptJob).toContain('scripts/debian/build-deb.sh "$VERSION"');
    expect(aptJob).toContain('scripts/debian/update-apt-repo.sh');
    expect(aptJob).toContain('git status --porcelain');
    expect(aptJob).not.toContain('scripts/debian/build-deb.sh "${{ steps.version.outputs.version }}"');
    expect(aptJob).not.toContain('git diff --quiet');
    expect(workflow).toContain("lsp-linux-x64");
    expect(workflow).toContain("lsp-linux-arm64");
  });

  test("Bitbucket Pipelines example installs Raven through the signed apt repo", () => {
    const example = readRepoFile("docs", "examples", "ci", "bitbucket-pipelines.yml");
    expect(example).toContain("image: ubuntu:24.04");
    expect(example).toContain("https://jbearak.github.io/apt-raven/raven-archive-keyring.gpg");
    expect(example).toContain("aaaee9d0c6d944091d1a78d8aeb4f93f59dc713ee1f218052add12b0d7c743cd");
    expect(example).toContain("sha256sum -c -");
    expect(example).toContain("deb [signed-by=/etc/apt/keyrings/raven-archive-keyring.gpg] https://jbearak.github.io/apt-raven stable main");
    expect(example).toContain("apt-get install -y raven");
    expect(example).toContain("raven packages update");
    expect(example).toContain("raven check");
  });

  test("CLI docs frame GitHub and Bitbucket setup as CI guidance", () => {
    const docs = readRepoFile("docs", "cli.md");
    expect(docs).toContain("## CI examples");
    expect(docs).toContain("### GitHub Actions example");
    expect(docs).toContain("### Bitbucket Pipelines example");
    expect(docs.indexOf("## CI examples")).toBeLessThan(docs.indexOf("### GitHub Actions example"));
    expect(docs.indexOf("### GitHub Actions example")).toBeLessThan(docs.indexOf("### Bitbucket Pipelines example"));
  });
});
