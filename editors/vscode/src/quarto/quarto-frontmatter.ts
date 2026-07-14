/**
 * Best-effort, per-document Shiny-server detection for Quarto commands.
 *
 * Only Quarto's `server: shiny` and `server: { type: shiny }` forms are
 * recognized. `runtime: shiny` is an R Markdown convention and MUST NOT
 * trigger this predicate. Project metadata and profiles are intentionally
 * outside this fast preflight; the Quarto CLI remains authoritative.
 */

import type { FrontmatterDoc } from '../knit/yaml-frontmatter';

/** Return true only for Quarto's two supported per-document Shiny forms. */
export function isShinyServerDocument(fm: FrontmatterDoc): boolean {
    if (fm.server === 'shiny') return true;
    if (fm.server === null || typeof fm.server !== 'object' || Array.isArray(fm.server)) {
        return false;
    }
    return (fm.server as Record<string, unknown>).type === 'shiny';
}
