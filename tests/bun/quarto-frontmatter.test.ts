import { describe, expect, test } from 'bun:test';
import { isShinyServerDocument } from '../../editors/vscode/src/quarto/quarto-frontmatter';

describe('isShinyServerDocument', () => {
    test('accepts scalar and nested Quarto Shiny server forms', () => {
        expect(isShinyServerDocument({ server: 'shiny' })).toBe(true);
        expect(isShinyServerDocument({ server: { type: 'shiny' } })).toBe(true);
    });

    test('does not borrow the R Markdown runtime convention', () => {
        expect(isShinyServerDocument({ runtime: 'shiny' })).toBe(false);
        expect(isShinyServerDocument({ runtime: 'shiny', server: 'knitr' })).toBe(false);
    });

    test('rejects unrelated and malformed server values', () => {
        expect(isShinyServerDocument({})).toBe(false);
        expect(isShinyServerDocument({ server: 'Shiny' })).toBe(false);
        expect(isShinyServerDocument({ server: { type: 'other' } })).toBe(false);
        expect(isShinyServerDocument({ server: ['shiny'] })).toBe(false);
        expect(isShinyServerDocument({ server: null })).toBe(false);
    });
});
