/** Resolve Quarto's reported output path against its two observed bases. */

import * as fs from 'fs';
import * as path from 'path';

/**
 * Quarto may report a relative output path from the source directory even when
 * Raven spawned it at the project root. Prefer an existing source-relative
 * file, then an existing cwd-relative file, and otherwise keep the more common
 * source-relative interpretation so the user can still act on the path.
 */
export function resolveQuartoRenderedOutputPath(
    reportedPath: string,
    sourceFsPath: string,
    cwd: string,
    exists: (candidate: string) => boolean = fs.existsSync,
): string {
    if (path.isAbsolute(reportedPath)) return reportedPath;
    const fromSource = path.resolve(path.dirname(sourceFsPath), reportedPath);
    if (exists(fromSource)) return fromSource;
    const fromCwd = path.resolve(cwd, reportedPath);
    return exists(fromCwd) ? fromCwd : fromSource;
}
