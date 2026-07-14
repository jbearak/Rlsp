import { describe, test, expect } from 'bun:test';
import * as fs from 'fs';
import * as path from 'path';

/**
 * Smoke test that every `contributes.grammars` entry in the VS Code
 * extension's `package.json` resolves to a real file under the
 * `editors/vscode/` tree.
 *
 * Codex review caught the case where vendored grammar files were
 * referenced from `package.json` but not actually committed — VS Code
 * would log a load failure at install time and silently fall back to
 * plain-text rendering. This test fails fast if any contribution
 * points at a missing file.
 *
 * Each grammar is also parsed as JSON and checked for the
 * `scopeName` declared in the manifest matching the `scopeName` in
 * the grammar file. A mismatch there would mean a TextMate registry
 * configured for one scope can't load the underlying grammar — the
 * exact failure mode shipping a stale `package.json` after a sync
 * would produce.
 */
const VSCODE_ROOT = path.resolve(__dirname, '..', '..', 'editors', 'vscode');
const PACKAGE_JSON = path.join(VSCODE_ROOT, 'package.json');

interface GrammarContribution {
    language?: string;
    scopeName: string;
    path: string;
    embeddedLanguages?: Record<string, string>;
    tokenTypes?: Record<string, string>;
}

function readContributions(): GrammarContribution[] {
    const raw = fs.readFileSync(PACKAGE_JSON, 'utf8');
    const pkg = JSON.parse(raw) as {
        contributes?: { grammars?: GrammarContribution[] };
    };
    return pkg.contributes?.grammars ?? [];
}

describe('contributes.grammars file paths', () => {
    const contributions = readContributions();

    test('at least one grammar is contributed', () => {
        // Sanity: catch a malformed parse before per-grammar tests.
        expect(contributions.length).toBeGreaterThan(0);
    });

    test('entries sharing a scopeName carry identical embedded-language metadata', () => {
        const byScope = new Map<string, GrammarContribution[]>();
        for (const grammar of contributions) {
            const entries = byScope.get(grammar.scopeName) ?? [];
            entries.push(grammar);
            byScope.set(grammar.scopeName, entries);
        }
        for (const entries of byScope.values()) {
            if (entries.length < 2) continue;
            const expectedEmbedded = entries[0].embeddedLanguages;
            const expectedTokenTypes = entries[0].tokenTypes;
            for (const grammar of entries.slice(1)) {
                expect(grammar.embeddedLanguages).toEqual(expectedEmbedded);
                expect(grammar.tokenTypes).toEqual(expectedTokenTypes);
            }
        }
    });

    for (const grammar of contributions) {
        const label = grammar.language ?? `inject:${grammar.scopeName}`;
        test(`${label} (${grammar.scopeName}) resolves to an existing file`, () => {
            const absolute = path.isAbsolute(grammar.path)
                ? grammar.path
                : path.join(VSCODE_ROOT, grammar.path);
            expect(fs.existsSync(absolute)).toBe(true);
        });

        test(`${label} (${grammar.scopeName}) grammar file declares the manifest's scopeName`, () => {
            const absolute = path.isAbsolute(grammar.path)
                ? grammar.path
                : path.join(VSCODE_ROOT, grammar.path);
            const text = fs.readFileSync(absolute, 'utf8');
            const parsed = JSON.parse(text) as { scopeName?: string };
            expect(parsed.scopeName).toBe(grammar.scopeName);
        });

        if (grammar.embeddedLanguages !== undefined) {
            test(`${label} (${grammar.scopeName}) every embeddedLanguages key is a scope the grammar file actually emits`, () => {
                const absolute = path.isAbsolute(grammar.path)
                    ? grammar.path
                    : path.join(VSCODE_ROOT, grammar.path);
                const parsed = JSON.parse(fs.readFileSync(absolute, 'utf8'));
                const emitted = collectEmittedEmbeddedScopes(parsed);
                const declared = Object.keys(grammar.embeddedLanguages ?? {});
                const orphans = declared.filter((s) => !emitted.has(s));
                // Failure here means the manifest declares an
                // `embeddedLanguages` mapping for a scope the grammar
                // does not actually emit — dead config that silently
                // misses its target.
                expect(orphans).toEqual([]);
            });
        }
    }
});

test('the quarto language contribution covers every .qmd casing', () => {
    const raw = fs.readFileSync(PACKAGE_JSON, 'utf8');
    const pkg = JSON.parse(raw) as {
        contributes?: {
            languages?: Array<{ id: string; extensions?: string[] }>;
        };
    };
    const quarto = pkg.contributes?.languages?.find(({ id }) => id === 'quarto');
    expect(quarto?.extensions).toEqual([
        '.qmd',
        '.qmD',
        '.qMd',
        '.qMD',
        '.Qmd',
        '.QmD',
        '.QMd',
        '.QMD',
    ]);
});

/**
 * Walk a parsed TextMate grammar and collect every `name` / `contentName`
 * value that looks like a `meta.embedded.*` scope. Those are the scope
 * names VS Code will actually see attached to tokens, which is what an
 * `embeddedLanguages` map keys on. Other `name`s (e.g.
 * `markup.fenced_code.block.markdown.rmarkdown`) are scope chains for
 * coloring and never trigger embedded-language behavior.
 */
function collectEmittedEmbeddedScopes(grammar: unknown): Set<string> {
    const out = new Set<string>();
    const walk = (v: unknown): void => {
        if (Array.isArray(v)) {
            for (const item of v) walk(item);
            return;
        }
        if (v && typeof v === 'object') {
            const obj = v as Record<string, unknown>;
            if (typeof obj.contentName === 'string'
                && obj.contentName.startsWith('meta.embedded.')) {
                out.add(obj.contentName);
            }
            if (typeof obj.name === 'string'
                && obj.name.startsWith('meta.embedded.')) {
                out.add(obj.name);
            }
            for (const value of Object.values(obj)) walk(value);
        }
    };
    walk(grammar);
    return out;
}
