/**
 * Pure project-key and working-directory discovery for Quarto commands.
 *
 * This is deliberately only Raven's keying/cwd heuristic. Quarto performs
 * its own richer project discovery from the target path. The walk is not
 * bounded by a VS Code workspace: it visits every ancestor through the
 * filesystem root and checks both supported project-file spellings through a
 * regular-file predicate; a directory named like a marker is not a project.
 */

import * as path from 'path';
import { canonicalOpKey } from '../knit/raven-knit-paths';

export interface QuartoProjectDeps {
    /** Synchronous regular-marker-file check, dependency-injected for pure tests. */
    isProjectMarkerFile(candidate: string): boolean;
}

export interface QuartoContext {
    /** Canonical operation key: project root when found, otherwise source file. */
    key: string;
    /** Spawn working directory. */
    cwd: string;
    /** Nearest `_quarto.yml` / `_quarto.yaml` ancestor, if any. */
    projectRoot: string | null;
}

/**
 * Find the nearest Quarto project marker at or above `startDir`.
 *
 * The root directory itself is checked before termination. The
 * `path.dirname(dir) === dir` condition is the only walk bound, which keeps
 * discovery correct for files outside the active workspace.
 */
export function findQuartoProjectRoot(
    startDir: string,
    deps: QuartoProjectDeps,
): string | null {
    let dir = path.resolve(startDir);
    for (;;) {
        if (
            deps.isProjectMarkerFile(path.join(dir, '_quarto.yml')) ||
            deps.isProjectMarkerFile(path.join(dir, '_quarto.yaml'))
        ) {
            return dir;
        }

        const parent = path.dirname(dir);
        if (parent === dir) return null;
        dir = parent;
    }
}

/**
 * Resolve the single source of truth for preview keying and spawn cwd.
 *
 * Keys reuse `canonicalOpKey` rather than duplicating its platform rules:
 * separators are normalized everywhere and Windows paths are lowercased.
 * The returned cwd/projectRoot retain normal filesystem path casing for
 * process spawning and display.
 */
export function resolveQuartoContext(
    fileFsPath: string,
    deps: QuartoProjectDeps,
): QuartoContext {
    const resolvedFile = path.resolve(fileFsPath);
    const fileDir = path.dirname(resolvedFile);
    const projectRoot = findQuartoProjectRoot(fileDir, deps);
    const keyPath = projectRoot ?? resolvedFile;
    return {
        key: canonicalOpKey({ fsPath: keyPath }),
        cwd: projectRoot ?? fileDir,
        projectRoot,
    };
}
