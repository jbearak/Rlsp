/** Resolve Quarto's reported output path against its two observed bases. */

import * as fs from 'fs';
import * as path from 'path';

/**
 * Quarto's relative output base varies by project type. Existing candidates
 * written at or after this render began are preferred, then ordered by newest
 * mtime. If clock skew or coarse timestamps leave no fresh candidate, newest
 * still wins; an exact tie prefers the cwd-relative project output. Missing
 * files retain the source-relative fallback so the user can still act on it.
 */
export function resolveQuartoRenderedOutputPath(
    reportedPath: string,
    sourceFsPath: string,
    cwd: string,
    renderStartMs: number,
    fileSystem: QuartoRenderedOutputFileSystem = defaultFileSystem,
): string {
    if (path.isAbsolute(reportedPath)) return reportedPath;
    const fromSource = path.resolve(path.dirname(sourceFsPath), reportedPath);
    const fromCwd = path.resolve(cwd, reportedPath);
    if (fromSource === fromCwd) return fromSource;

    const candidates = [
        { path: fromSource, mtimeMs: fileMtimeMs(fileSystem, fromSource) },
        { path: fromCwd, mtimeMs: fileMtimeMs(fileSystem, fromCwd) },
    ].filter((candidate) => candidate.mtimeMs !== null);
    if (candidates.length === 0) return fromSource;
    if (candidates.length === 1) return candidates[0].path;

    const fresh = candidates.filter((candidate) => (
        candidate.mtimeMs! >= renderStartMs
    ));
    const eligible = fresh.length > 0 ? fresh : candidates;
    // cwd is last, so >= makes an exact mtime tie project-output-preferred.
    return eligible.reduce((newest, candidate) => (
        candidate.mtimeMs! >= newest.mtimeMs! ? candidate : newest
    )).path;
}

export interface QuartoRenderedOutputFileSystem {
    exists(candidate: string): boolean;
    mtimeMs(candidate: string): number;
}

const defaultFileSystem: QuartoRenderedOutputFileSystem = {
    exists: fs.existsSync,
    mtimeMs: (candidate) => fs.statSync(candidate).mtimeMs,
};

function fileMtimeMs(
    fileSystem: QuartoRenderedOutputFileSystem,
    candidate: string,
): number | null {
    if (!fileSystem.exists(candidate)) return null;
    try {
        const value = fileSystem.mtimeMs(candidate);
        return Number.isFinite(value) ? value : Number.NEGATIVE_INFINITY;
    } catch {
        return Number.NEGATIVE_INFINITY;
    }
}
