/// <reference types="mocha" />

import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';

interface MenuEntry {
    command?: string;
    submenu?: string;
    when?: string;
}

interface PackageJson {
    contributes: {
        menus: Record<string, MenuEntry[]>;
    };
}

const packageJsonPath = path.resolve(__dirname, '..', '..', 'package.json');

function loadPackageJson(): PackageJson {
    return JSON.parse(fs.readFileSync(packageJsonPath, 'utf8')) as PackageJson;
}

suite('Quarto menu gating', () => {
    test('editor-title submenu uses case-insensitive qmd gating only', () => {
        const pkg = loadPackageJson();
        const entry = pkg.contributes.menus['editor/title']
            .find((candidate) => candidate.submenu === 'raven.quarto');
        assert.ok(entry);
        const when = entry.when ?? '';
        assert.ok(when.includes('resourceExtname =~ /\\.qmd$/i'), when);
        assert.ok(!when.includes('raven.rConsoleEnabled'), when);
        assert.ok(!when.includes('quarto.quarto'), when);
    });

    test('Preview and Render require trust while Stop does not', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.quarto'];
        for (const command of ['raven.quarto.preview', 'raven.quarto.render']) {
            const entry = entries.find((candidate) => candidate.command === command);
            assert.ok(entry, `missing ${command}`);
            assert.ok(entry.when?.includes('isWorkspaceTrusted'), entry.when);
        }
        const stop = entries.find((candidate) =>
            candidate.command === 'raven.quarto.stopPreview');
        assert.ok(stop);
        assert.ok(!(stop.when ?? '').includes('isWorkspaceTrusted'));
    });

    test('command palette mirrors qmd and trust gates; output stays ungated', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus.commandPalette;
        for (const command of ['raven.quarto.preview', 'raven.quarto.render']) {
            const entry = entries.find((candidate) => candidate.command === command);
            assert.ok(entry);
            assert.ok(entry.when?.includes('resourceExtname =~ /\\.qmd$/i'));
            assert.ok(entry.when?.includes('isWorkspaceTrusted'));
        }
        const stop = entries.find((candidate) =>
            candidate.command === 'raven.quarto.stopPreview');
        assert.ok(stop?.when?.includes('resourceExtname =~ /\\.qmd$/i'));
        assert.ok(!stop?.when?.includes('isWorkspaceTrusted'));
        const output = entries.find((candidate) =>
            candidate.command === 'raven.quarto.openOutputChannel');
        assert.ok(output);
        assert.strictEqual(output.when, undefined);
    });
});
