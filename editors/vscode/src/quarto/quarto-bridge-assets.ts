/** Defensive loading for the optional packaged Quarto theme bridge. */

import * as fs from 'fs';
import * as path from 'path';
import type { QuartoPreviewBridgeAssets } from './quarto-preview-proxy';

type ReadFile = (path: fs.PathOrFileDescriptor) => Buffer;

/**
 * Load both bridge files atomically, or disable theming without aborting
 * Quarto registration when an installation omitted either dist asset.
 */
export function loadQuartoPreviewBridgeAssets(
    extensionFsPath: string,
    output: { appendLine(value: string): unknown },
    readFile: ReadFile = fs.readFileSync,
): QuartoPreviewBridgeAssets | undefined {
    try {
        const bridgeDir = path.join(
            extensionFsPath,
            'dist',
            'quarto-theme-bridge',
        );
        return {
            javascript: readFile(path.join(bridgeDir, 'bridge.js')),
            css: readFile(path.join(bridgeDir, 'bridge.css')),
        };
    } catch (error) {
        try {
            output.appendLine(
                '[quarto] Theme bridge assets unavailable; previews will be unthemed: ' +
                errorMessage(error),
            );
        } catch {
            // Registration must remain available even if output is disposing.
        }
        return undefined;
    }
}

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}
