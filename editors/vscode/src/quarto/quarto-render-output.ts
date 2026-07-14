/** Resolve Quarto's reported output path against its two observed bases. */

import * as fs from 'fs';
import * as path from 'path';

/**
 * Quarto's relative output base varies by project type. When both the source-
 * relative and cwd-relative files exist, the render's newest mtime identifies
 * the file it just wrote and avoids opening a stale peer. Ties and missing
 * files fall back to the source-relative interpretation.
 */
export function resolveQuartoRenderedOutputPath(
    reportedPath: string,
    sourceFsPath: string,
    cwd: string,
    fileSystem: QuartoRenderedOutputFileSystem = defaultFileSystem,
): string {
    if (path.isAbsolute(reportedPath)) return reportedPath;
    const fromSource = path.resolve(path.dirname(sourceFsPath), reportedPath);
    const fromCwd = path.resolve(cwd, reportedPath);
    if (fromSource === fromCwd) return fromSource;

    const sourceExists = fileSystem.exists(fromSource);
    const cwdExists = fileSystem.exists(fromCwd);
    if (sourceExists && !cwdExists) return fromSource;
    if (cwdExists && !sourceExists) return fromCwd;
    if (!sourceExists && !cwdExists) return fromSource;

    return safeMtimeMs(fileSystem, fromCwd) > safeMtimeMs(fileSystem, fromSource)
        ? fromCwd
        : fromSource;
}

export interface QuartoRenderedOutputFileSystem {
    exists(candidate: string): boolean;
    mtimeMs(candidate: string): number;
}

const defaultFileSystem: QuartoRenderedOutputFileSystem = {
    exists: fs.existsSync,
    mtimeMs: (candidate) => fs.statSync(candidate).mtimeMs,
};

function safeMtimeMs(
    fileSystem: QuartoRenderedOutputFileSystem,
    candidate: string,
): number {
    try {
        const value = fileSystem.mtimeMs(candidate);
        return Number.isFinite(value) ? value : Number.NEGATIVE_INFINITY;
    } catch {
        return Number.NEGATIVE_INFINITY;
    }
}
