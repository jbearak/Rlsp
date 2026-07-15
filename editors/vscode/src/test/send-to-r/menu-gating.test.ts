/// <reference types="mocha" />

import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';

declare const suite: Mocha.SuiteFunction;
declare const test: Mocha.TestFunction;

interface MenuEntry {
    command?: string;
    submenu?: string;
    when?: string;
    group?: string;
}

interface Keybinding {
    command: string;
    key?: string;
    mac?: string;
    when?: string;
}

interface PackageJson {
    contributes: {
        menus: Record<string, MenuEntry[]>;
        keybindings: Keybinding[];
    };
}

const packageJsonPath = path.resolve(__dirname, '..', '..', '..', 'package.json');

function loadPackageJson(): PackageJson {
    return JSON.parse(fs.readFileSync(packageJsonPath, 'utf8')) as PackageJson;
}

// Regex literals for the extension families — these mirror the regex
// fragments embedded in the package.json `when` expressions so the tests
// stay locked to a single source of truth.
const RMD_QMD_EXT = /\\\.\(rmd\|Rmd\|RMD\|rmarkdown\|Rmarkdown\|RMARKDOWN\|qmd\|Qmd\|QMD\)\$/;
const RMD_ONLY_EXT = /\\\.\(rmd\|Rmd\|RMD\|rmarkdown\|Rmarkdown\|RMARKDOWN\)\$/;
const QMD_ONLY_EXT = /\\\.qmd\$\/i/;

function findCommand(entries: MenuEntry[], command: string): MenuEntry | undefined {
    return entries.find((entry) => entry.command === command);
}

function findSubmenu(entries: MenuEntry[], submenu: string): MenuEntry | undefined {
    return entries.find((entry) => entry.submenu === submenu);
}

function assertHidesForRmdQmd(entry: MenuEntry, label: string): void {
    const when = entry.when ?? '';
    // The auto-include entries must hide on .Rmd / .Rmarkdown / .qmd. We require both
    // the explicit `editorLangId == r` constraint and the negated extension
    // test — otherwise an .R file named `notes.Rmd` (matched only by
    // extension) would slip through with `editorLangId == r` alone.
    assert.ok(
        when.includes('editorLangId == r'),
        `${label} must restrict to editorLangId == r, got: ${when}`,
    );
    assert.ok(
        when.includes('!(resourceExtname'),
        `${label} must exclude resourceExtname matches, got: ${when}`,
    );
    assert.ok(
        RMD_QMD_EXT.test(when),
        `${label} must reference the .Rmd/.Rmarkdown/.qmd extension family, got: ${when}`,
    );
}

function assertShowsOnlyForRmdQmd(entry: MenuEntry, label: string): void {
    const when = entry.when ?? '';
    assert.ok(
        when.includes('editorLangId == rmd'),
        `${label} must fire for editorLangId == rmd, got: ${when}`,
    );
    assert.ok(
        when.includes('editorLangId == quarto'),
        `${label} must fire for editorLangId == quarto, got: ${when}`,
    );
    // Also covers .R files saved with a .Rmd / .Rmarkdown / .qmd extension (handy when
    // another extension claims the language).
    assert.ok(
        when.includes('editorLangId == r'),
        `${label} must also fire for editorLangId == r when paired with an extension test, got: ${when}`,
    );
    assert.ok(
        RMD_QMD_EXT.test(when),
        `${label} must reference the .Rmd/.Rmarkdown/.qmd extension family, got: ${when}`,
    );
}

suite('Send to R submenu: editor-title gating', () => {
    test('auto-include line entries and Source File hide on .Rmd / .Rmarkdown / .qmd', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.sendToR'] ?? [];
        for (const command of ['raven.runUpwardLines', 'raven.runDownwardLines', 'raven.sourceFile']) {
            const entry = findCommand(entries, command);
            assert.ok(entry, `raven.sendToR must contain ${command}`);
            assertHidesForRmdQmd(entry, command);
        }
    });

    test('Run Line or Selection stays visible for every supported language', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.sendToR'] ?? [];
        const entry = findCommand(entries, 'raven.runLineOrSelection');
        assert.ok(entry, 'raven.sendToR must contain raven.runLineOrSelection');
        // Run Line or Selection sends what the user explicitly picked (the
        // selection, or statement detection on the cursor line). That stays
        // useful inside a chunk too, so the entry has no `when` clause.
        assert.strictEqual(
            entry.when,
            undefined,
            'raven.runLineOrSelection must have no `when` clause so it surfaces on .R, .Rmd, .Rmarkdown, and .qmd alike',
        );
    });

    test('chunk commands are gated to .Rmd / .Rmarkdown / .qmd', () => {
        // Plain `.R` cell mode (`# %%`) is real, but the toolbar menu is
        // meant for chunk-based authoring; users with cell-mode `.R` reach
        // chunk operations through the CodeLens or command palette. Gating
        // here keeps the .R toolbar lean.
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.sendToR'] ?? [];
        for (const command of [
            'raven.runCurrentChunk',
            'raven.runCurrentChunkAndMove',
            'raven.runAboveChunks',
            'raven.runBelowChunks',
            'raven.runAllChunks',
        ]) {
            const entry = findCommand(entries, command);
            assert.ok(entry, `raven.sendToR must contain ${command}`);
            assertShowsOnlyForRmdQmd(entry, command);
        }
    });

    test('Knit is gated to R Markdown files and the rmdKnit feature flag', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.sendToR'] ?? [];
        const entry = findCommand(entries, 'raven.knit');
        assert.ok(entry, 'raven.sendToR must contain raven.knit');
        const when = entry.when ?? '';
        assert.ok(
            when.includes('raven.rmdKnit.enabled'),
            `Knit must require raven.rmdKnit.enabled, got: ${when}`,
        );
        // Knit only applies to R Markdown — Quarto preview is a separate
        // concern — so the extension test must reference the R Markdown-only
        // family and must not pull in .qmd extensions.
        assert.ok(
            RMD_ONLY_EXT.test(when),
            `Knit must reference the R Markdown-only extension family, got: ${when}`,
        );
        assert.ok(
            !when.includes('qmd'),
            `Knit must not reference .qmd extensions, got: ${when}`,
        );
    });

    test('Terminal submenu surfaces for every supported language', () => {
        // The Terminal submenu sends to whatever terminal is currently
        // active (tmux, Docker, …); it must stay reachable on .R, .Rmd,
        // .Rmarkdown, and .qmd. The auto-include and Source-File entries
        // inside the submenu have their own gating — see the dedicated suite below.
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.sendToR'] ?? [];
        const submenu = findSubmenu(entries, 'raven.sendToR.terminal');
        assert.ok(submenu, 'raven.sendToR must reference the Terminal submenu');
        assert.strictEqual(
            submenu.when,
            undefined,
            'raven.sendToR.terminal submenu reference must have no `when` clause',
        );
    });
});

suite('Send to R → Terminal submenu: editor-title gating', () => {
    test('terminal auto-include and sourceFile entries hide on .Rmd / .Rmarkdown / .qmd', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.sendToR.terminal'] ?? [];
        for (const command of [
            'raven.terminal.runUpwardLines',
            'raven.terminal.runDownwardLines',
            'raven.terminal.sourceFile',
        ]) {
            const entry = findCommand(entries, command);
            assert.ok(entry, `raven.sendToR.terminal must contain ${command}`);
            assertHidesForRmdQmd(entry, command);
        }
    });

    test('Terminal: Run Line or Selection stays visible for every supported language', () => {
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.sendToR.terminal'] ?? [];
        const entry = findCommand(entries, 'raven.terminal.runLineOrSelection');
        assert.ok(entry, 'raven.sendToR.terminal must contain raven.terminal.runLineOrSelection');
        assert.strictEqual(
            entry.when,
            undefined,
            'raven.terminal.runLineOrSelection must have no `when` clause so it surfaces on .R, .Rmd, .Rmarkdown, and .qmd alike',
        );
    });

    test('terminal chunk commands mirror the main chunk set on .Rmd / .Rmarkdown / .qmd', () => {
        // The Terminal submenu should expose the same chunk operations the
        // main menu does, so a user driving a tmux-hosted R session can run
        // chunks without bouncing through the managed R terminal. Knit is
        // intentionally excluded — knitr renders documents, not interactive
        // chunks, and it always uses the managed flow.
        const pkg = loadPackageJson();
        const entries = pkg.contributes.menus['raven.sendToR.terminal'] ?? [];
        for (const command of [
            'raven.terminal.runCurrentChunk',
            'raven.terminal.runCurrentChunkAndMove',
            'raven.terminal.runAboveChunks',
            'raven.terminal.runBelowChunks',
            'raven.terminal.runAllChunks',
        ]) {
            const entry = findCommand(entries, command);
            assert.ok(entry, `raven.sendToR.terminal must contain ${command}`);
            assertShowsOnlyForRmdQmd(entry, command);
        }
    });

    test('terminal chunk commands are declared in package.json', () => {
        // Mirrors the main-menu chunk command set so the runtime registration
        // in register_chunk_commands has matching `contributes.commands`
        // entries. Without these the command palette would not surface them.
        const pkg = loadPackageJson() as unknown as {
            contributes: { commands: Array<{ command: string; title: string; category?: string }> };
        };
        const declared = new Set(pkg.contributes.commands.map((c) => c.command));
        for (const command of [
            'raven.terminal.runCurrentChunk',
            'raven.terminal.runCurrentChunkAndMove',
            'raven.terminal.runAboveChunks',
            'raven.terminal.runBelowChunks',
            'raven.terminal.runAllChunks',
        ]) {
            assert.ok(
                declared.has(command),
                `${command} must be declared in package.json contributes.commands`,
            );
        }
    });
});

suite('File-level Shift+Enter chord', () => {
    test('Knit takes Shift+Enter on R Markdown files when the rmdKnit feature flag is on', () => {
        // The .R Shift+Enter shortcut runs `source()`. Knit is the closest
        // equivalent for an R Markdown document, so the chord is repurposed there
        // whenever the feature flag is on.
        const pkg = loadPackageJson();
        const bindings = pkg.contributes.keybindings ?? [];
        const entry = bindings.find(
            (b) =>
                b.command === 'raven.knit'
                && b.key === 'ctrl+shift+enter'
                && b.mac === 'cmd+shift+enter',
        );
        assert.ok(
            entry,
            'expected a raven.knit keybinding bound to ctrl+shift+enter / cmd+shift+enter',
        );
        const when = entry.when ?? '';
        assert.ok(
            when.includes('raven.rmdKnit.enabled'),
            `raven.knit keybinding must require raven.rmdKnit.enabled, got: ${when}`,
        );
        assert.ok(
            when.includes('editorLangId == rmd'),
            `raven.knit keybinding must fire for editorLangId == rmd, got: ${when}`,
        );
        assert.ok(
            RMD_ONLY_EXT.test(when),
            `raven.knit keybinding must reference the R Markdown-only extension family, got: ${when}`,
        );
        // Knit must not steal the chord on .qmd — Quarto preview belongs to
        // a separate command path.
        assert.ok(
            !when.includes('quarto'),
            `raven.knit keybinding must not fire for editorLangId == quarto, got: ${when}`,
        );
        assert.ok(
            !when.includes('qmd'),
            `raven.knit keybinding must not reference .qmd extensions, got: ${when}`,
        );
    });

    test('Knit keybinding requires an R Markdown extension on every branch', () => {
        // A `.R` file whose language was overridden to `rmd` would satisfy
        // `editorLangId == rmd` alone — but Knit invokes knitr against the
        // physical file path and breaks if the on-disk extension is not
        // R Markdown. The `when` clause must require the extension match outside
        // any language-id alternation so no `editorLangId == rmd || …`
        // branch can short-circuit the extension test.
        const pkg = loadPackageJson();
        const bindings = pkg.contributes.keybindings ?? [];
        const entry = bindings.find(
            (b) =>
                b.command === 'raven.knit'
                && b.key === 'ctrl+shift+enter'
                && b.mac === 'cmd+shift+enter',
        );
        assert.ok(entry, 'expected a raven.knit keybinding bound to the Shift+Enter chord');
        const when = entry.when ?? '';
        // Reject any pattern like `editorLangId == rmd || …` that would
        // accept `editorLangId == rmd` on its own. The extension test must
        // sit outside the language alternation: a top-level `&&` joining
        // `resourceExtname =~ …` to the language clause.
        const top_level_and_with_ext = /&&\s*resourceExtname =~/;
        assert.ok(
            top_level_and_with_ext.test(when),
            `raven.knit keybinding must AND the R Markdown extension test at the top level — not inside a language-id alternation. Got: ${when}`,
        );
    });

    test('Quarto Preview takes Shift+Enter on qmd files', () => {
        const pkg = loadPackageJson();
        const bindings = pkg.contributes.keybindings ?? [];
        const entry = bindings.find(
            (b) =>
                b.command === 'raven.quarto.preview'
                && b.key === 'ctrl+shift+enter'
                && b.mac === 'cmd+shift+enter',
        );
        assert.ok(
            entry,
            'expected a raven.quarto.preview keybinding bound to ctrl+shift+enter / cmd+shift+enter',
        );
        const when = entry.when ?? '';
        assert.ok(
            when.includes('editorTextFocus'),
            `raven.quarto.preview keybinding must require editor text focus, got: ${when}`,
        );
        assert.ok(
            when.includes('isWorkspaceTrusted'),
            `raven.quarto.preview keybinding must require workspace trust, got: ${when}`,
        );
        assert.ok(
            QMD_ONLY_EXT.test(when),
            `raven.quarto.preview keybinding must be restricted to .qmd files, got: ${when}`,
        );
        assert.ok(
            when.includes('raven.rConsoleEnabled'),
            `raven.quarto.preview keybinding must require R-console activation, got: ${when}`,
        );
    });

    test('Shift+Enter chord is shared only by the three file-level actions', () => {
        // Source File, Knit Preview, and Quarto Preview have mutually exclusive
        // file-extension gates, so the chord dispatches by document type.
        const pkg = loadPackageJson();
        const bindings = pkg.contributes.keybindings ?? [];
        const sharers = bindings
            .filter(
                (b) =>
                    b.key === 'ctrl+shift+enter'
                    || b.key === 'shift+ctrl+enter'
                    || b.mac === 'cmd+shift+enter'
                    || b.mac === 'shift+cmd+enter',
            )
            .map((b) => b.command)
            .sort();
        assert.deepStrictEqual(
            sharers,
            ['raven.knit', 'raven.quarto.preview', 'raven.sourceFile'],
            'only Source File, Knit Preview, and Quarto Preview may share the Shift+Enter chord',
        );
    });

    test('Source File keybinding stays restricted to plain .R', () => {
        // Pin the existing Source File gating so it can coexist with the
        // new Knit binding on the same chord without double-firing.
        // On R Markdown files with Knit disabled the chord is intentionally
        // unbound. Quarto Preview owns the chord on .qmd files.
        const pkg = loadPackageJson();
        const bindings = pkg.contributes.keybindings ?? [];
        const entry = bindings.find((b) => b.command === 'raven.sourceFile');
        assert.ok(entry, 'expected a raven.sourceFile keybinding');
        const when = entry.when ?? '';
        assert.ok(
            when.includes('editorLangId == r'),
            `raven.sourceFile must restrict to editorLangId == r, got: ${when}`,
        );
        assert.ok(
            when.includes('!(resourceExtname'),
            `raven.sourceFile must exclude .Rmd/.Rmarkdown/.qmd extensions, got: ${when}`,
        );
        assert.ok(
            RMD_QMD_EXT.test(when),
            `raven.sourceFile must reference the .Rmd/.Rmarkdown/.qmd extension family, got: ${when}`,
        );
    });

    test('runCurrentChunk no longer holds the Shift+Enter chord', () => {
        // Previously raven.runCurrentChunk was bound to cmd+shift+enter for
        // .Rmd / .Rmarkdown / .qmd. With Knit owning the chord on R Markdown, the previous
        // binding must be removed (not duplicated) so it cannot double-fire.
        const pkg = loadPackageJson();
        const bindings = pkg.contributes.keybindings ?? [];
        const conflict = bindings.find(
            (b) =>
                b.command === 'raven.runCurrentChunk'
                && (b.key === 'ctrl+shift+enter' || b.mac === 'cmd+shift+enter'),
        );
        assert.strictEqual(
            conflict,
            undefined,
            'raven.runCurrentChunk must not hold the Shift+Enter chord — that chord belongs to Source File, Knit Preview, or Quarto Preview according to file type',
        );
    });
});
