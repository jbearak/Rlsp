/**
 * Pure path / hash helpers for the per-session knit temp-dir layout.
 *
 * Layout:
 *   <os.tmpdir()>/raven-knit/<workspaceHash>/<sessionId>/
 *     preview/<sourceHash>/   ← stable per R Markdown source, scoped to this VS Code window
 *     export/<uuid>/          ← throwaway, per editor-toolbar export invocation
 *
 * Keeping this module free of `vscode` imports lets it be unit-tested
 * under `bun test`.
 */

import * as path from 'path';
import * as crypto from 'crypto';
import * as fs from 'fs';
import * as os from 'os';

interface UriLike {
    fsPath: string;
}

/**
 * Stable per-document key used by the OperationController registry.
 *
 * Normalizes path separators (so `./foo.Rmd` and `foo.Rmd` collapse) and
 * lowercases on Windows (NTFS is case-insensitive, so the same file under
 * different casings must collapse to one controller). On POSIX-like
 * filesystems case is preserved.
 */
export function canonicalOpKey(uri: UriLike, platform: NodeJS.Platform = process.platform): string {
    const normalized = path.normalize(uri.fsPath);
    return platform === 'win32' ? normalized.toLowerCase() : normalized;
}

/** SHA-256 of the workspace folder's URI (string). Stable per workspace. */
export function computeWorkspaceHash(workspaceUri: string): string {
    return crypto.createHash('sha256').update(workspaceUri).digest('hex');
}

/** SHA-256 of an absolute R Markdown source path. Stable per source file. */
export function computeSourceHash(absPath: string): string {
    return crypto.createHash('sha256').update(absPath).digest('hex');
}

/**
 * True when `absPath` resolves to a path inside `root` (or equal to it).
 * Used to gate YAML-supplied CSS paths against traversal-escape attacks.
 *
 * Resolves symlinks on both sides via `fs.realpathSync.native` before
 * the containment comparison so a symlink at `workspace/css/x.css`
 * pointing to `/etc/passwd` is rejected — a syntactic check on the
 * lexical path would have passed. If either side cannot be realpath'd
 * (file does not exist, EACCES on a parent directory), we fall back to
 * the lexical comparison; the only realistic case is `absPath` pointing
 * at a not-yet-created CSS file, which Pandoc will reject downstream.
 */
export function isUnderContainmentRoot(absPath: string, root: string): boolean {
    let resolvedAbs = path.normalize(absPath);
    let resolvedRoot = path.normalize(root);
    try {
        resolvedAbs = fs.realpathSync.native(resolvedAbs);
    } catch {
        // Leaf may not exist yet; keep the lexical form. The dirname
        // check below catches symlink escape via an intermediate dir
        // even when the leaf is missing.
        try {
            const parent = path.dirname(resolvedAbs);
            const base = path.basename(resolvedAbs);
            resolvedAbs = path.join(fs.realpathSync.native(parent), base);
        } catch {
            // Parent unreachable too — fall through to lexical check.
        }
    }
    try {
        resolvedRoot = fs.realpathSync.native(resolvedRoot);
    } catch {
        // Root must exist for containment to be meaningful, but if it
        // doesn't the lexical compare is the conservative fallback.
    }
    const rel = path.relative(resolvedRoot, resolvedAbs);
    if (rel === '') return true;
    return !rel.startsWith('..') && !path.isAbsolute(rel);
}

/**
 * Root under which every per-session knit artifact lives:
 * `<os.tmpdir()>/raven-knit`. The single source of truth for this path —
 * `sessionRoot`, the activation sweep, the deactivation cleanup, and the
 * persistence helpers all derive from it.
 */
export function ravenKnitRoot(): string {
    return path.join(os.tmpdir(), 'raven-knit');
}

/** Root of all per-session artifacts: `<tmp>/raven-knit/<workspaceHash>/<sessionId>/`. */
export function sessionRoot(workspaceHash: string, sessionId: string): string {
    return path.join(ravenKnitRoot(), workspaceHash, sessionId);
}

/** `<sessionRoot>/preview/<sourceHash>/`. Stable per R Markdown source for the session. */
export function previewDirFor(workspaceHash: string, sessionId: string, sourceHash: string): string {
    return path.join(sessionRoot(workspaceHash, sessionId), 'preview', sourceHash);
}

/** `<sessionRoot>/export/<uuid>/`. Throwaway per editor-toolbar export. */
export function exportDirFor(workspaceHash: string, sessionId: string, uuid: string): string {
    return path.join(sessionRoot(workspaceHash, sessionId), 'export', uuid);
}

export interface PreviewArtifactPaths {
    previewDir: string;
    mdPath: string;
    htmlPath: string;
    figDir: string;
    /** SHA-256 of the absolute R Markdown source path. Used as the refcount key in OperationRegistry. */
    previewKey: string;
}

/**
 * Resolve every per-source artifact path for the given R Markdown source.
 *
 * Requires `initSessionState` to have been called (i.e., the extension
 * is active). The optional `sessionInfo` parameter accepts an explicit
 * `(workspaceHash, sessionId)` pair so unit tests can drive the
 * function without going through `session-state.ts` — production
 * callers should omit it and let the function consult the live session.
 *
 * `workspaceHash` is resolved via `workspaceHashFor(rmdAbsPath)` when
 * no explicit pair is given, which means single-file mode (no
 * workspace) cleanly falls back to a per-source-parent-dir hash.
 */
export function previewArtifactPaths(
    rmdAbsPath: string,
    sessionInfo?: { workspaceHash: string; sessionId: string },
): PreviewArtifactPaths {
    const sourceHash = computeSourceHash(rmdAbsPath);
    const resolved = sessionInfo ?? resolveSessionForSource(rmdAbsPath);
    const previewDir = previewDirFor(resolved.workspaceHash, resolved.sessionId, sourceHash);
    const baseName = path
        .basename(rmdAbsPath)
        .replace(/\.(?:[Rr][Mm][Dd]|[Rr][Mm][Aa][Rr][Kk][Dd][Oo][Ww][Nn])$/, '');
    return {
        previewDir,
        mdPath: path.join(previewDir, `${baseName}.md`),
        htmlPath: path.join(previewDir, `${baseName}.html`),
        figDir: path.join(previewDir, 'figure'),
        previewKey: sourceHash,
    };
}

/**
 * Late-bound import to avoid a circular dependency between
 * `raven-knit-paths` and `session-state` (the latter already imports
 * `computeSourceHash` from this module).
 */
function resolveSessionForSource(rmdAbsPath: string): { workspaceHash: string; sessionId: string } {
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const { currentSession, workspaceHashFor } = require('./session-state') as typeof import('./session-state');
    return { workspaceHash: workspaceHashFor(rmdAbsPath), sessionId: currentSession().sessionId };
}
