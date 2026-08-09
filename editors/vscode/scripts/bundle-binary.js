const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const binDir = path.join(__dirname, '..', 'bin');
const binaryName = process.platform === 'win32' ? 'raven.exe' : 'raven';
const destBinary = path.join(binDir, binaryName);
const repoRoot = path.join(__dirname, '..', '..', '..');
const srcBinary = path.join(repoRoot, 'target', 'release', binaryName);

if (!fs.existsSync(binDir)) {
    fs.mkdirSync(binDir, { recursive: true });
}

// The Tier 3 sidecar (names.db) is deliberately NOT bundled into the VSIX (it
// is also excluded in .vscodeignore). VS Code users run alongside their local R
// install, so Tier 1 resolves their installed packages directly — they don't
// need the broad CRAN/Bioconductor floor, and it would only bloat the VSIX.

function copyAndChmod(message = 'Bundled raven binary') {
    fs.copyFileSync(srcBinary, destBinary);
    fs.chmodSync(destBinary, 0o755);
    console.log(message);
}

// When target/release is available, refresh the bundled binary if it is newer.
// Use size as a tie-breaker for rebuilds on filesystems with coarse timestamps.
if (fs.existsSync(srcBinary)) {
    if (fs.existsSync(destBinary)) {
        const srcStat = fs.statSync(srcBinary);
        const destStat = fs.statSync(destBinary);
        const sourceIsNewer = srcStat.mtimeMs > destStat.mtimeMs;
        const tiedTimestampChangedSize =
            srcStat.mtimeMs === destStat.mtimeMs && srcStat.size !== destStat.size;
        if (!sourceIsNewer && !tiedTimestampChangedSize) {
            console.log('Bundled raven binary is already current');
            process.exit(0);
        }
        copyAndChmod('Refreshed stale raven binary');
    } else {
        copyAndChmod();
    }
    process.exit(0);
}

// Without a matching target/release binary, preserve either pre-placed name.
// Cross-platform packaging (e.g. win32 on Linux) may not match process.platform.
if (fs.existsSync(path.join(binDir, 'raven')) || fs.existsSync(path.join(binDir, 'raven.exe'))) {
    console.log('Preserving pre-placed raven binary; no matching target/release binary found');
    process.exit(0);
}

// No pre-bundled binary, no target/release build. Try cargo build before
// giving up — this is what dev / pretest needs so `bun run pretest` can
// produce a working LSP without the developer remembering to run cargo
// manually. `RAVEN_BUNDLE_NO_BUILD=1` opts out for environments that
// must not invoke cargo (e.g. CI release pipelines that build the binary
// in a separate step).
if (process.env.RAVEN_BUNDLE_NO_BUILD === '1') {
    console.error('raven binary not found and RAVEN_BUNDLE_NO_BUILD=1 — refusing to build. Run: cargo build --release -p raven');
    process.exit(1);
}

console.log(`raven binary not found at ${srcBinary}; running "cargo build --release -p raven"…`);
const result = spawnSync('cargo', ['build', '--release', '-p', 'raven'], {
    cwd: repoRoot,
    stdio: 'inherit',
});

if (result.error && result.error.code === 'ENOENT') {
    console.error('cargo not found in PATH. Install Rust (https://rustup.rs) or set RAVEN_BUNDLE_NO_BUILD=1 and provide a pre-built binary.');
    process.exit(1);
}
if (result.status !== 0) {
    console.error(`cargo build --release -p raven failed (exit ${result.status}).`);
    process.exit(result.status || 1);
}

if (!fs.existsSync(srcBinary)) {
    console.error(`cargo build succeeded but ${srcBinary} is missing. This is a build-script bug.`);
    process.exit(1);
}

copyAndChmod();
