// Two-pass esbuild: extension bundle + webview bundles.
const fs = require('fs');
const path = require('path');
const esbuild = require('esbuild');
// eslint-disable-next-line @typescript-eslint/no-var-requires
const sveltePlugin = require('esbuild-svelte').default ?? require('esbuild-svelte');
// eslint-disable-next-line @typescript-eslint/no-var-requires
const sveltePreprocess = require('svelte-preprocess').default ?? require('svelte-preprocess');

const root = path.resolve(__dirname, '..');
const dist = path.join(root, 'dist');
const reactProductionDefine = { 'process.env.NODE_ENV': '"production"' };

// `jsonc-parser` ships both a UMD `main` (`./lib/umd/main.js`) and an ESM
// `module` (`./lib/esm/main.js`). esbuild's Node platform defaults to
// `mainFields = ['main']`, which picks the UMD build — whose wrapper uses
// dynamic `require("./impl/format")` / `./impl/edit` / etc. calls that
// esbuild can't statically follow. The bundle compiles but throws
// "Cannot find module './impl/format'" at activation time. Aliasing
// `jsonc-parser` directly to its ESM entry gives esbuild plain `import`
// statements it can inline, and keeps the fix scoped — resolution for
// every other runtime dependency (apache-arrow, vscode-languageclient,
// …) stays on its default `main` field.
const jsoncParserEsmEntry = require.resolve('jsonc-parser/lib/esm/main.js');

async function buildExtension() {
    await esbuild.build({
        entryPoints: [path.join(root, 'src', 'extension.ts')],
        bundle: true,
        platform: 'node',
        target: 'node18',
        format: 'cjs',
        external: ['vscode'],
        sourcemap: true,
        outfile: path.join(dist, 'extension.js'),
        logLevel: 'info',
        alias: { 'jsonc-parser': jsoncParserEsmEntry },
    });
}

/**
 * Build a Svelte webview bundle.
 *
 * @param {string} name   - Output directory name under dist/webviews/ (e.g. 'plot-viewer').
 * @param {string} entry  - Absolute path to the webview entry point (main.ts).
 */
async function buildSvelteWebview(name, entry) {
    const webviewDist = path.join(dist, 'webviews', name);
    await esbuild.build({
        entryPoints: [entry],
        bundle: true,
        platform: 'browser',
        target: 'chrome108',
        format: 'iife',
        mainFields: ['svelte', 'browser', 'module', 'main'],
        conditions: ['svelte', 'browser'],
        plugins: [
            sveltePlugin({
                preprocess: sveltePreprocess(),
                compilerOptions: { css: 'external' },
            }),
        ],
        loader: { '.css': 'css' },
        sourcemap: true,
        outfile: path.join(webviewDist, 'index.js'),
        logLevel: 'info',
    });
}

/**
 * Build a React webview bundle.
 *
 * @param {string} name   - Output directory name under dist/webviews/ (e.g. 'data-viewer').
 * @param {string} entry  - Absolute path to the webview entry point (main.tsx).
 */
async function buildReactWebview(name, entry) {
    const webviewDist = path.join(dist, 'webviews', name);
    await esbuild.build({
        entryPoints: [entry],
        bundle: true,
        platform: 'browser',
        target: 'chrome108',
        format: 'iife',
        define: reactProductionDefine,
        loader: { '.css': 'css' },
        minify: true,
        sourcemap: true,
        outfile: path.join(webviewDist, 'index.js'),
        logLevel: 'info',
    });
}

/**
 * Copy the bundled `onig.wasm` from `vscode-oniguruma` into `dist/` so
 * the Knit Output rendering pipeline can load it at runtime via
 * `extensionUri`. esbuild can't bundle .wasm into a CJS module, and
 * `vscode-oniguruma`'s `loadWASM(buffer)` accepts the raw bytes — we
 * read the file at runtime and pass them in. Keeping the asset in
 * `dist/` (rather than `node_modules/`) ensures it stays in the
 * shipped VSIX and resolves correctly from the bundled extension.
 */
function copyOnigWasm() {
    const src = require.resolve('vscode-oniguruma/release/onig.wasm');
    const dest = path.join(dist, 'onig.wasm');
    if (!fs.existsSync(dist)) fs.mkdirSync(dist, { recursive: true });
    fs.copyFileSync(src, dest);
    console.log(`Copied ${path.relative(root, src)} -> ${path.relative(root, dest)}`);
}

/** Copy the injected Quarto bridge into the shipped runtime-only dist tree. */
function copyQuartoThemeBridge() {
    const src = path.join(root, 'src', 'quarto', 'bridge');
    const dest = path.join(dist, 'quarto-theme-bridge');
    fs.mkdirSync(dest, { recursive: true });
    for (const asset of ['bridge.js', 'bridge.css']) {
        fs.copyFileSync(path.join(src, asset), path.join(dest, asset));
    }
    console.log('Copied src/quarto/bridge -> dist/quarto-theme-bridge');
}

(async () => {
    try {
        await Promise.all([
            buildExtension(),
            buildSvelteWebview(
                'plot-viewer',
                path.join(root, 'src', 'plot', 'webview', 'main.ts'),
            ),
            buildSvelteWebview(
                'help-viewer',
                path.join(root, 'src', 'help', 'webview', 'main.ts'),
            ),
            buildReactWebview(
                'data-viewer',
                path.join(root, 'src', 'data-viewer', 'webview', 'main.tsx'),
            ),
        ]);
        copyOnigWasm();
        copyQuartoThemeBridge();
    } catch (err) {
        console.error(err);
        process.exit(1);
    }
})();
