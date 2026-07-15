import { describe, expect, test } from 'bun:test';
import { loadQuartoPreviewBridgeAssets } from '../../editors/vscode/src/quarto/quarto-bridge-assets';

describe('Quarto bridge asset registration loading', () => {
    test('a missing packaged asset logs once and leaves registration unbridged', () => {
        const lines: string[] = [];
        const load = () => loadQuartoPreviewBridgeAssets(
            '/missing-extension',
            { appendLine: (line) => { lines.push(line); } },
            () => { throw new Error('injected ENOENT'); },
        );

        let result: ReturnType<typeof load>;
        expect(() => { result = load(); }).not.toThrow();
        expect(result!).toBeUndefined();
        expect(lines).toEqual([
            '[quarto] Theme bridge assets unavailable; previews will be unthemed: ' +
            'injected ENOENT',
        ]);
    });
});
