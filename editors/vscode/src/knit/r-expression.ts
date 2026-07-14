/**
 * Construction and pre-validation of the R expression we hand to
 * `R --no-save --no-restore -e <expr>` when knitting an .Rmd file.
 *
 * Two guards protect this surface:
 *
 *   1. `validatePathForRExpression` rejects bytes that can't safely round-
 *      trip through an R single-quoted string literal: NUL (terminates R
 *      strings silently), most C0 controls (mangle the R expression's
 *      diagnostic output and have no legitimate place in a path), and
 *      DEL. Tab is allowed because some filesystems permit it.
 *   2. `validateFormatIdentifier` enforces a strict allow-pattern for the
 *      YAML output-format value: identifier chars plus `::`, `.`, and
 *      `-`. The HTML-only pipeline no longer passes this to R, but we
 *      still validate its shape — defense in depth, since the YAML
 *      parser already hands us a string from a controlled grammar.
 *
 * `escapeRString` itself is straightforward (backslash and apostrophe
 * escapes inside single quotes); the bulk of the security work is the
 * validation step that runs first. We never construct shell strings —
 * the caller spawns R with an argv array — so this is R-literal
 * injection prevention, not shell-injection prevention.
 */

export class ValidatePathError extends Error {
    constructor(message: string) {
        super(message);
        this.name = 'ValidatePathError';
    }
}

export class ValidateFormatError extends Error {
    constructor(message: string) {
        super(message);
        this.name = 'ValidateFormatError';
    }
}

/**
 * Refuse strings that can't safely become an R single-quoted literal:
 * NUL, DEL, and C0 controls other than tab. Tab is the one C0 we keep
 * because filesystems permit it and it's harmless inside an R string.
 * Bidi-override and other exotic printable Unicode characters are NOT
 * rejected — they round-trip cleanly through `escapeRString`.
 */
export function validatePathForRExpression(value: string): void {
    if (typeof value !== 'string') {
        throw new ValidatePathError('Path must be a string.');
    }
    if (value.length === 0) {
        throw new ValidatePathError('Path must not be empty.');
    }
    for (let i = 0; i < value.length; i++) {
        const code = value.charCodeAt(i);
        if (code === 0x09) continue; // tab: allowed
        if (code === 0x00) {
            throw new ValidatePathError('Path contains a NUL byte.');
        }
        if (code < 0x20) {
            throw new ValidatePathError(
                `Path contains control character U+${code.toString(16).padStart(4, '0').toUpperCase()}.`,
            );
        }
        if (code === 0x7F) {
            throw new ValidatePathError('Path contains the DEL character.');
        }
    }
}

const FORMAT_IDENTIFIER_PATTERN = /^[A-Za-z0-9_:.-]+$/;

/**
 * Enforce the strict allow-pattern for the YAML output-format value. The
 * current HTML-only pipeline no longer passes this to R (see
 * `KnitInput.format`); we still validate its shape so a malformed value
 * short-circuits before we spawn R, and so the check stays correct for
 * any future format-passing path. The pattern covers what YAML front
 * matter legitimately produces (`html_document`, `bookdown::pdf_document2`,
 * `default`, `all`, ...) and rejects anything that could break out of an
 * R identifier or call into a different function.
 */
export function validateFormatIdentifier(value: string): void {
    if (typeof value !== 'string' || value.length === 0) {
        throw new ValidateFormatError('Format identifier must be a non-empty string.');
    }
    if (!FORMAT_IDENTIFIER_PATTERN.test(value)) {
        throw new ValidateFormatError(
            `Format identifier "${value}" contains characters outside [A-Za-z0-9_:.-].`,
        );
    }
}

/**
 * Escape a string for inclusion inside an R single-quoted literal.
 * Single quotes and backslashes become `\'` and `\\`; the result is
 * wrapped in single quotes. Callers MUST validate inputs (NUL/control
 * characters) before calling — escaping alone cannot rescue those.
 */
export function escapeRString(value: string): string {
    if (typeof value !== 'string' || value.length === 0) {
        throw new ValidatePathError('Cannot escape an empty or non-string value.');
    }
    let out = "'";
    for (let i = 0; i < value.length; i++) {
        const ch = value[i];
        if (ch === '\\') out += '\\\\';
        else if (ch === "'") out += "\\'";
        else out += ch;
    }
    out += "'";
    return out;
}

import type { ChunkOpts } from './output-options';

export interface KnitExpressionInput {
    /** Absolute path of the .Rmd file being knitted. */
    filePath: string;
    /**
     * The intermediate Markdown file knitr writes. We pass it
     * explicitly so the post-knit renderer doesn't have to guess
     * where knitr's default-derived path landed.
     */
    outputPath: string;
    /**
     * Working directory for chunk evaluation. `null` means inherit
     * the subprocess CWD (the user's `current` mode).
     */
    knitRootDir: string | null;
    /**
     * The validated output-format identifier from YAML. The HTML-only
     * pipeline doesn't pass this to R (knitr::knit doesn't care about
     * formats; the .md → .html step is ours), but we keep it on the
     * input shape so the caller's gate-check + validation remain
     * symmetric with the previous `rmarkdown::render` flow. The
     * format is still validated for shape so a bad value short-
     * circuits before we spawn R.
     */
    format: string;
    /**
     * Base directory for knitr's plot output (figures land relative to
     * this directory). Set via `opts_knit$set(base.dir = …)` — distinct
     * from `root.dir`, which is where chunks run. This lets us direct
     * figures to a temp folder while leaving the user's chunks running
     * in their configured working directory.
     */
    baseDir: string;
    /**
     * Relative path inside `baseDir` where figures go, e.g. 'figure/'.
     * Set via `opts_chunk$set(fig.path = …)`. Must include the trailing
     * separator to match knitr's expectation (it concatenates this
     * prefix directly with chunk labels).
     */
    figPath: string;
    /**
     * Optional knitr chunk-level overrides from YAML's `output:` block
     * (`fig_width`, `fig_height`, `fig_retina`, `dpi`, `dev`). Applied
     * via a single `opts_chunk$set(...)` call before `knitr::knit`.
     */
    chunkOpts: ChunkOpts;
}

/**
 * Whitelisted plot device strings — anything else from YAML is rejected
 * before reaching `escapeRString`, so a malicious `dev` value cannot
 * carry an arbitrary R string into the subprocess even if the front-
 * matter parser somehow let it through.
 */
const DEV_ALLOWLIST: ReadonlySet<string> = new Set([
    'png', 'pdf', 'svg', 'jpeg', 'cairo_pdf',
]);

/**
 * Build the single-line R expression passed to `R -e`.
 *
 * The new pipeline calls `knitr::knit` directly — not
 * `rmarkdown::render` — because we render the resulting markdown to
 * HTML ourselves via VS Code's `markdown.api.render`. This skips
 * pandoc entirely for HTML output.
 *
 * Behavior:
 *   - `knitr::opts_knit$set(root.dir = …)` mirrors what
 *     `rmarkdown::render(knit_root_dir = …)` did before. When
 *     `knitRootDir` is null we pin `root.dir` to `getwd()` so chunk
 *     evaluation happens in the subprocess CWD (matches the `current`
 *     mode contract).
 *   - knitr writes to `<outputPath>.tmp` and we `file.rename` to the
 *     real `outputPath` on success. R's `file.rename` is atomic on
 *     POSIX and uses `MoveFileEx(MOVEFILE_REPLACE_EXISTING)` on
 *     Windows. A cancelled / failed / timed-out knit leaves the
 *     partial `.md.tmp` behind but the destination `.md` either
 *     contains the previous successful output or doesn't exist — so
 *     downstream consumers (webview Export ▾) cannot pick up a
 *     half-written file.
 *   - `envir = new.env()` isolates chunk evaluation, matching
 *     rmarkdown's default.
 *   - `quiet = TRUE` suppresses knitr's per-chunk progress output;
 *     the output channel still records the final `Output created:`
 *     line we emit.
 *   - `cat('Output created: …')` keeps the existing
 *     `parseRenderedOutputPath` contract — the classifier doesn't
 *     care that knitr (not pandoc) is the producer. We emit the
 *     real `output` path, not the `.tmp` sidecar.
 *
 * Each interpolated value is validated before escaping (see module
 * docstring), so the caller can rely on a clean throw rather than a
 * half-escaped string if the inputs are wrong.
 */
export function buildKnitExpression(input: KnitExpressionInput): string {
    validatePathForRExpression(input.filePath);
    validatePathForRExpression(input.outputPath);
    validatePathForRExpression(input.baseDir);
    validatePathForRExpression(input.figPath);
    validateFormatIdentifier(input.format);
    if (input.knitRootDir !== null) {
        validatePathForRExpression(input.knitRootDir);
    }
    if (input.chunkOpts.dev !== undefined && !DEV_ALLOWLIST.has(input.chunkOpts.dev)) {
        throw new ValidatePathError(`Chunk dev value not in allowlist: ${input.chunkOpts.dev}`);
    }

    const rootDirLiteral = input.knitRootDir !== null
        ? escapeRString(input.knitRootDir)
        : 'getwd()';

    // One long expression, multiple statements joined with `;` so R
    // can run it under a single `-e`. We wrap in `local({…})` to
    // contain bindings and to keep the `out` variable from leaking.
    const inputLit = escapeRString(input.filePath);
    const outputLit = escapeRString(input.outputPath);
    const baseDirLit = escapeRString(input.baseDir);
    const figPathLit = escapeRString(input.figPath);

    // YAML-supplied chunk-level options pass through R-side variables
    // rather than being interpolated directly into the
    // `opts_chunk$set(...)` argument list. Even though every value we
    // emit here is already validated (numeric finiteness + dev
    // allowlist), the variable indirection matches the contract in
    // CLAUDE.md's "R subprocess safety" invariant and gives one audit
    // point for any future chunk option we add.
    const assigns: string[] = [];
    const namedArgs: string[] = [];
    const co = input.chunkOpts;
    if (co.fig_width !== undefined && Number.isFinite(co.fig_width)) {
        assigns.push(`.raven_fig_width <- ${co.fig_width}`);
        namedArgs.push('fig.width = .raven_fig_width');
    }
    if (co.fig_height !== undefined && Number.isFinite(co.fig_height)) {
        assigns.push(`.raven_fig_height <- ${co.fig_height}`);
        namedArgs.push('fig.height = .raven_fig_height');
    }
    if (co.fig_retina !== undefined && Number.isFinite(co.fig_retina)) {
        assigns.push(`.raven_fig_retina <- ${co.fig_retina}`);
        namedArgs.push('fig.retina = .raven_fig_retina');
    }
    if (co.dpi !== undefined && Number.isInteger(co.dpi)) {
        assigns.push(`.raven_dpi <- ${co.dpi}L`);
        namedArgs.push('dpi = .raven_dpi');
    }
    if (co.dev !== undefined) {
        // co.dev passed DEV_ALLOWLIST above; `escapeRString` is the
        // single-quoted-literal wrapper. The assignment puts it on the
        // R side; the `opts_chunk$set` call references the local var.
        assigns.push(`.raven_dev <- ${escapeRString(co.dev)}`);
        namedArgs.push('dev = .raven_dev');
    }
    const yamlOptsChunk = assigns.length > 0
        ? ` ${assigns.join('; ')}; knitr::opts_chunk$set(${namedArgs.join(', ')});`
        : '';

    return [
        'local({',
        // Force `knitr::include_graphics()` to keep the path it was
        // handed rather than relativizing an absolute path against the
        // output directory. knitr's default (`rel_path = TRUE`) rewrites
        // an absolute image path to one relative to the output dir and
        // then re-checks it from `root.dir`; when the image lives
        // elsewhere in the workspace that rewritten path is unreachable
        // and the knit hard-errors ("Cannot find the file(s): …"). With
        // `rel_path = FALSE` the emitted `<img src>` stays absolute and
        // the panel's data-URL inliner resolves it (see
        // `inlineLocalImagesAsDataUrls`). This is a subprocess-local
        // option, so it never leaks into the user's interactive session.
        ' options(knitr.graphics.rel_path = FALSE);',
        ` knitr::opts_knit$set(root.dir = ${rootDirLiteral}, base.dir = ${baseDirLit});`,
        ` knitr::opts_chunk$set(fig.path = ${figPathLit});`,
        yamlOptsChunk,
        ` .raven_output <- ${outputLit};`,
        ` .raven_tmp_output <- paste0(.raven_output, '.tmp');`,
        ` knitr::knit(`,
        `input = ${inputLit},`,
        ` output = .raven_tmp_output,`,
        ` envir = new.env(),`,
        ` quiet = TRUE);`,
        // file.rename returns FALSE (with a warning) on failure rather
        // than erroring. If we ignored that, a failed rename would
        // leave the previous .md in place while we still emit
        // "Output created:" — `parseRenderedOutputPath` would then
        // hand a stale file to the caller. Convert the FALSE return
        // into a hard stop so the knit outcome reflects reality.
        //
        // The leading `.` on every `.raven_*` name is mandatory: R
        // identifiers cannot start with `_`, so `__raven_*` would
        // fail at parse time with "unexpected input in ...". The dot
        // prefix is R's idiom for "internal / hidden" and is what
        // `__` was unsuccessfully trying to be.
        ` if (!isTRUE(file.rename(.raven_tmp_output, .raven_output)))`,
        ` stop('Failed to rename ', .raven_tmp_output, ' to ', .raven_output);`,
        ` cat('Output created: ', .raven_output, '\\n', sep = '')`,
        ' })',
    ].join('');
}
