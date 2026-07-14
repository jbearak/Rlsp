/**
 * Asynchronous project-key and working-directory discovery for Quarto commands.
 *
 * This is deliberately only Raven's keying/cwd heuristic. Quarto performs
 * its own richer project discovery from the target path. The walk is not
 * bounded by a VS Code workspace: it visits every ancestor through the
 * filesystem root and checks both supported project-file spellings through an
 * async regular-file predicate; a directory named like a marker is not a
 * project. The source is realpathed before the walk, so symlink aliases outside
 * a project classify identically to the physical source within it. Filesystem
 * failures retain the lexical path as a safe fallback.
 */

import * as path from 'path';
import { canonicalOpKey } from '../knit/raven-knit-paths';

export interface QuartoProjectDeps {
    /** Async source realpath, dependency-injected for pure tests. */
    realpath(candidate: string): Promise<string>;
    /** Async regular-marker-file check, dependency-injected for pure tests. */
    isProjectMarkerFile(candidate: string): Promise<boolean>;
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
export async function findQuartoProjectRoot(
    startDir: string,
    deps: QuartoProjectDeps,
): Promise<string | null> {
    let dir = path.resolve(startDir);
    for (;;) {
        const [hasYml, hasYaml] = await Promise.all([
            deps.isProjectMarkerFile(path.join(dir, '_quarto.yml')),
            deps.isProjectMarkerFile(path.join(dir, '_quarto.yaml')),
        ]);
        if (hasYml || hasYaml) {
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
export async function resolveQuartoContext(
    fileFsPath: string,
    deps: QuartoProjectDeps,
): Promise<QuartoContext> {
    const lexicalFile = path.resolve(fileFsPath);
    let resolvedFile = lexicalFile;
    try {
        resolvedFile = path.resolve(await deps.realpath(lexicalFile));
    } catch {
        // Missing/broken paths retain lexical behavior; Quarto reports them.
    }
    const fileDir = path.dirname(resolvedFile);
    const projectRoot = await findQuartoProjectRoot(fileDir, deps);
    const keyPath = projectRoot ?? resolvedFile;
    return {
        key: canonicalOpKey({ fsPath: keyPath }),
        cwd: projectRoot ?? fileDir,
        projectRoot,
    };
}
