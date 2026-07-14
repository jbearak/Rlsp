/**
 * Filesystem adapter for pure Quarto project discovery.
 *
 * Project markers must be regular files. Missing paths, directories named
 * `_quarto.yml` / `_quarto.yaml`, and filesystem errors all mean “not a
 * marker”; the pure ancestor walk receives only this boolean predicate.
 */

import * as fs from 'fs';

/** Return whether `candidate` is a regular Quarto project-marker file. */
export function isQuartoProjectMarkerFile(candidate: string): boolean {
    try {
        return fs.statSync(candidate).isFile();
    } catch {
        return false;
    }
}
