/// <reference types="mocha" />

import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';

interface MenuEntry {
    command?: string;
    submenu?: string;
    when?: string;
    group?: string;
}

interface CommandEntry {
    command: string;
    icon?: string;
}

interface SubmenuEntry {
    id: string;
}

interface PackageJson {
    contributes: {
        commands: CommandEntry[];
        submenus: SubmenuEntry[];
        menus: Record<string, MenuEntry[]>;
    };
}

const packageJsonPath = path.resolve(__dirname, '..', '..', 'package.json');

function loadPackageJson(): PackageJson {
    return JSON.parse(fs.readFileSync(packageJsonPath, 'utf8')) as PackageJson;
}

function normalizedWhen(entry: MenuEntry | undefined): string | undefined {
    return entry?.when?.replace(/\s+/g, ' ').trim();
}

const QMD_TRUST_GATE =
    'raven.rConsoleEnabled && isWorkspaceTrusted && resourceExtname =~ /\\.qmd$/i';
const QMD_GATE = 'resourceExtname =~ /\\.qmd$/i';
const RMD_KNIT_GATE =
    'raven.rmdKnit.enabled && resourceExtname =~ ' +
    '/\\.(rmd|Rmd|RMD|rmarkdown|Rmarkdown|RMARKDOWN)$/';

suite('Quarto menu gating', () => {
    test('removes the standalone Quarto submenu', () => {
        const pkg = loadPackageJson();
        assert.strictEqual(
            pkg.contributes.submenus.some(({ id }) => id === 'raven.quarto'),
            false,
        );
        assert.strictEqual(pkg.contributes.menus['raven.quarto'], undefined);
        assert.strictEqual(
            pkg.contributes.menus['editor/title']
                .some(({ submenu }) => submenu === 'raven.quarto'),
            false,
        );
    });

    test('editor-title provides exact Quarto and Knit preview buttons', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['editor/title'];
        const quarto = entries.find(({ command }) => command === 'raven.quarto.preview');
        const knit = entries.find(({ command }) => command === 'raven.knit');

        assert.strictEqual(normalizedWhen(quarto), QMD_TRUST_GATE);
        assert.strictEqual(quarto?.group, 'navigation@5');
        assert.ok(normalizedWhen(quarto)?.includes('raven.rConsoleEnabled'));
        assert.ok(!normalizedWhen(quarto)?.includes('quarto.quarto'));
        assert.strictEqual(normalizedWhen(knit), RMD_KNIT_GATE);
        assert.strictEqual(knit?.group, 'navigation@5');
    });

    test('preview commands carry editor-title icons', () => {
        const commands = loadPackageJson().contributes.commands;
        for (const id of ['raven.quarto.preview', 'raven.knit']) {
            const command = commands.find((candidate) => candidate.command === id);
            assert.ok(command, `missing ${id}`);
            assert.strictEqual(command.icon, '$(preview)');
        }
    });

    test('Send to R contains exactly-gated Quarto actions', () => {
        const entries = loadPackageJson().contributes.menus['raven.sendToR'];
        const expected = [
            ['raven.quarto.preview', QMD_TRUST_GATE, '5_quarto@1'],
            ['raven.quarto.render', QMD_TRUST_GATE, '5_quarto@2'],
            ['raven.quarto.stopPreview', QMD_GATE, '5_quarto@3'],
        ] as const;
        for (const [command, when, group] of expected) {
            const entry = entries.find((candidate) => candidate.command === command);
            assert.ok(entry, `missing ${command}`);
            assert.strictEqual(normalizedWhen(entry), when);
            assert.strictEqual(entry.group, group);
        }
    });

    test('command palette mirrors R-console, qmd, and trust gates; output stays ungated', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus.commandPalette;
        for (const command of ['raven.quarto.preview', 'raven.quarto.render']) {
            const entry = entries.find((candidate) => candidate.command === command);
            assert.ok(entry);
            assert.strictEqual(normalizedWhen(entry), QMD_TRUST_GATE);
        }
        const stop = entries.find((candidate) =>
            candidate.command === 'raven.quarto.stopPreview');
        assert.strictEqual(normalizedWhen(stop), QMD_GATE);
        const output = entries.find((candidate) =>
            candidate.command === 'raven.quarto.openOutputChannel');
        assert.ok(output);
        assert.strictEqual(output.when, undefined);
    });
});
