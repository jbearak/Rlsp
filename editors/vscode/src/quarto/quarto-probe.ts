/**
 * Pure identity check for the Quarto CLI resolver.
 *
 * `quarto --version` prints only a bare version number, so it cannot
 * distinguish Quarto from an unrelated executable. Quarto's public
 * `--help` output contains the stable product marker `Quarto CLI`.
 */

/** Returns true iff `stdout` contains Quarto's `--help` identity marker. */
export function isQuartoHelpOutput(stdout: string): boolean {
    return stdout.includes('Quarto CLI');
}
