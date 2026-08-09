const path = require('path');
const { spawnSync } = require('child_process');

const extensionRoot = path.join(__dirname, '..');

function targetNeedsPreplaced(target, platform = process.platform, arch = process.arch) {
    const hostTarget = `${platform}-${arch}`;
    return Boolean(target) && target !== hostTarget;
}

function run(command, args, env) {
    const result = spawnSync(command, args, {
        cwd: extensionRoot,
        env,
        stdio: 'inherit',
    });
    if (result.error) {
        throw result.error;
    }
    if (result.status !== 0) {
        process.exit(result.status || 1);
    }
}

function main() {
    const args = process.argv.slice(2);
    const env = { ...process.env };

    // A cross-target invocation necessarily relies on a binary placed in bin/;
    // protect it both here and in the `vscode:prepublish` lifecycle that VSCE
    // invokes. Same-host packaging keeps the normal freshness refresh. The
    // release workflow's explicit marker still wins for its same-host target.
    if (targetNeedsPreplaced(args[0])) {
        env.RAVEN_BUNDLE_PREPLACED = '1';
    }

    run('bun', ['run', 'bundle'], env);
    run('bun', ['run', 'copy-binary'], env);
    run('bun', ['run', 'copy-notice'], env);

    // Invoke the JavaScript entry point through Node rather than spawning the
    // platform shim (`vsce.cmd` on Windows), which is not directly executable
    // by spawnSync without a shell.
    const vsce = path.join(extensionRoot, 'node_modules', '@vscode', 'vsce', 'vsce');
    run(process.execPath, [vsce, 'package', '--target', ...args], env);
}

if (require.main === module) {
    main();
}

module.exports = { targetNeedsPreplaced };
