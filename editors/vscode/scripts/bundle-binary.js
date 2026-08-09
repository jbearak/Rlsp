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

// A deliberately pre-placed binary always wins, and is never compared against
// `target/release`.
//
// The release workflow cross-compiles for every target, unzips the correct
// artifact into `bin/`, and only then runs `vsce package` (which triggers this
// script). On a runner that also has a host `target/release/raven` — a warm
// cache, a self-hosted runner, a local `vsce package` after a dev build — a
// freshness comparison would happily overwrite the cross-compiled binary with
// the host one, shipping a VSIX whose server cannot execute on the platform it
// claims to target. Content freshness cannot distinguish that case: the host
// binary is legitimately newer.
//
// `RAVEN_BUNDLE_PREPLACED=1` marks the pre-placed case explicitly. Both target
// names are checked because cross-platform packaging (e.g. win32 on Linux)
// means `process.platform` does not match the target.
if (
    process.env.RAVEN_BUNDLE_PREPLACED === '1' &&
    (fs.existsSync(path.join(binDir, 'raven')) || fs.existsSync(path.join(binDir, 'raven.exe')))
) {
    console.log('Preserving pre-placed raven binary (RAVEN_BUNDLE_PREPLACED=1)');
    process.exit(0);
}

// Note there is deliberately NO guard here for a pre-placed binary under the
// OTHER platform's filename. It would be redundant — the copy below writes to
// `destBinary`, this host's name, so a foreign-named artifact is untouched
// either way — and actively harmful: `copy-binary` also runs from `pretest` and
// ordinary dev builds, where exiting early would skip creating the host binary
// and leave the extension with no runnable server while reporting success.
//
// Refresh from target/release when it is newer. Size is a tie-breaker for
// rebuilds on filesystems with coarse timestamps.
//
// Neither mtime nor size is a content hash, so a rebuild that produces
// different bytes at the same length AND the same timestamp is not detected.
// That is vanishingly unlikely for a compiled binary (both must collide), and
// the cost of being wrong here is a stale dev/test binary, not a bad release —
// releases take the pre-placed path above. `cargo build` also updates mtime on
// every relink, so the normal rebuild path is always caught.
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

// No matching target/release binary — preserve whatever is already bundled.
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
