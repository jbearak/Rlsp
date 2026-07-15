/**
 * Best-effort parser for the rendered-output path(s) printed by the knit
 * R expression.
 *
 * Source-of-truth message: Raven's knit expression (built in
 * `r-expression`) emits `Output created: <path>` via `cat()` to stdout.
 * The caller passes the subprocess's combined stdout+stderr, so a line R
 * happens to route through `message()` is still matched. If parsing
 * fails we surface "Knit succeeded (output path unknown)" rather than
 * fabricating a path — the subprocess exit code is the ground truth for
 * success/failure; output parsing is a UX nicety.
 *
 * The single-output HTML pipeline emits exactly one line, but we return
 * every match defensively so a future multi-output path could offer
 * "Show All" alongside opening the first.
 */

const OUTPUT_LINE = /^[\t ]*Output created:[\t ]+(.+?)[\t ]*$/;
// The knit expression also emits the effective `root.dir` (see
// `r-expression`) so the preview can resolve relative
// `include_graphics` images against the base knitr actually used.
const ROOT_LINE = /^[\t ]*Raven-knit-root:[\t ]+(.+?)[\t ]*$/;

export interface ParsedOutput {
    paths: string[];
    /**
     * The effective knit `root.dir` the subprocess reported, or `null`
     * if the line was absent (older expression, or knit that never
     * reached the emit). The last match wins, matching R's last write.
     */
    rootDir: string | null;
}

export function parseRenderedOutputPath(stdout: string): ParsedOutput {
    const paths: string[] = [];
    let rootDir: string | null = null;
    for (const rawLine of stdout.split('\n')) {
        const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
        const outputMatch = line.match(OUTPUT_LINE);
        if (outputMatch) {
            paths.push(outputMatch[1]);
            continue;
        }
        const rootMatch = line.match(ROOT_LINE);
        if (rootMatch) rootDir = rootMatch[1];
    }
    return { paths, rootDir };
}
