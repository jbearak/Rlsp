/**
 * Filesystem adapter for pure Quarto project discovery.
 *
 * Project markers must be regular files. Missing paths, directories named
 * `_quarto.yml` / `_quarto.yaml`, and filesystem errors all mean “not a
 * marker”; the pure async ancestor walk receives only this boolean predicate.
 */

import * as fs from 'fs';

/** Return whether `candidate` is a regular Quarto project-marker file. */
export async function isQuartoProjectMarkerFile(candidate: string): Promise<boolean> {
    try {
        return (await fs.promises.stat(candidate)).isFile();
    } catch {
        return false;
    }
}
