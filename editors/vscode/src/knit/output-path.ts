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

export interface ParsedOutput {
    paths: string[];
}

export function parseRenderedOutputPath(stdout: string): ParsedOutput {
    const paths: string[] = [];
    for (const rawLine of stdout.split('\n')) {
        const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
        const match = line.match(OUTPUT_LINE);
        if (match) paths.push(match[1]);
    }
    return { paths };
}
