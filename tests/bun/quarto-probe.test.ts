import { describe, expect, test } from 'bun:test';
import { isQuartoHelpOutput } from '../../editors/vscode/src/quarto/quarto-probe';

describe('isQuartoHelpOutput', () => {
    test('accepts Quarto help output containing its identity marker', () => {
        expect(isQuartoHelpOutput('Quarto CLI\n\nUsage: quarto [options]')).toBe(true);
        expect(isQuartoHelpOutput('Commands for the Quarto CLI tool')).toBe(true);
    });

    test('rejects bare version and unrelated help output', () => {
        expect(isQuartoHelpOutput('1.9.38\n')).toBe(false);
        expect(isQuartoHelpOutput('Usage: pandoc [OPTIONS]')).toBe(false);
        expect(isQuartoHelpOutput('')).toBe(false);
    });

    test('uses the exact case-sensitive product marker', () => {
        expect(isQuartoHelpOutput('quarto cli')).toBe(false);
        expect(isQuartoHelpOutput('QUARTO CLI')).toBe(false);
    });
});
