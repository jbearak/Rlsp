import { describe, test, expect } from 'bun:test';
import * as fs from 'fs';
import * as path from 'path';

/**
 * Behavior test for the Tier 1 `onEnterRules` in the R language
 * configuration — specifically the assignment-operator rule added for
 * issue #611 (a line ending in `<-` / `<<-` indents one level even with
 * Tier 2 off).
 *
 * The rule's regex must fire only when the operator is real code: the
 * leading group consumes characters where `#` may appear only inside a
 * completed string or backtick identifier, so an operator inside a
 * comment (whole-line, roxygen, or trailing) never matches, while
 * legitimate lines with `#` inside strings still do.
 *
 * VS Code evaluates `beforeText` as a JavaScript regex against the text
 * before the cursor, so compiling and testing with `RegExp` here
 * exercises the exact semantics the editor uses.
 */
const LANGUAGE_CONFIG = path.resolve(
    __dirname,
    '..',
    '..',
    'editors',
    'vscode',
    'language-configuration.json',
);

interface OnEnterRule {
    beforeText: string;
    action: { indent: string };
}

function loadRules(): OnEnterRule[] {
    const config = JSON.parse(fs.readFileSync(LANGUAGE_CONFIG, 'utf8'));
    return config.onEnterRules as OnEnterRule[];
}

function assignmentRule(): RegExp {
    const rule = loadRules().find((r) => r.beforeText.includes('<<-'));
    expect(rule).toBeDefined();
    expect(rule!.action.indent).toBe('indent');
    return new RegExp(rule!.beforeText);
}

describe('assignment onEnterRule (#611)', () => {
    const fires = [
        'x <-',
        '  x <<-',
        'x <- # comment',
        'x <-  ',
        'result.value <-',
        'x[grepl("#", y)] <-',
        '`column#name` <-',
        'x[grepl("a\\"b", y)] <-',
    ];

    const doesNotFire = [
        '# x <-',
        "#' x <-",
        '  # indented comment x <-',
        'x <- 1 # old form was y <-',
        'x <- y',
        'x <= y',
        'x = 1',
        'x <- "a <-"',
        'f(x =',
    ];

    test('fires on assignment-terminated code lines', () => {
        const re = assignmentRule();
        for (const line of fires) {
            expect(re.test(line), `should fire: ${JSON.stringify(line)}`).toBe(true);
        }
    });

    test('does not fire on comments or non-assignment lines', () => {
        const re = assignmentRule();
        for (const line of doesNotFire) {
            expect(re.test(line), `should not fire: ${JSON.stringify(line)}`).toBe(false);
        }
    });

    test('all onEnterRules compile as JavaScript regexes', () => {
        for (const rule of loadRules()) {
            expect(() => new RegExp(rule.beforeText)).not.toThrow();
        }
    });

    test('known limitation: multiline-string interiors match', () => {
        // `beforeText` sees one line with no cross-line state, so a line
        // ending in `<-` inside an unterminated multiline string matches.
        // Every existing operator rule (`%>%`, `+`, `~`, `%infix%`) shares
        // this class of false positive; Tier 2 corrects it when active.
        // Pinned so a future fix is deliberate rather than accidental.
        const re = assignmentRule();
        expect(re.test('looks like <-')).toBe(true);
    });

    test('known limitation: raw strings are not recognized', () => {
        // R raw strings (`r"(...)"` with any number of dashes and three
        // delimiter pairs) are beyond a single regex alternative, so a raw
        // string containing `"` and `#` before the operator is a false
        // NEGATIVE — the rule stays silent and Tier 2 (whose tokenizer does
        // understand raw strings) supplies the indent when active. Pinned
        // so a future fix is deliberate rather than accidental.
        const re = assignmentRule();
        expect(re.test('x[r"(a"#b)"] <-')).toBe(false);
    });
});

/**
 * `rmd`/`quarto` use `rmd-language-configuration.json`, a copy of the R
 * configuration WITHOUT the assignment onEnterRule: `beforeText` regexes
 * apply to the whole document, and a prose paragraph ending in `<-` (e.g.
 * documentation about the operator) must not indent the next Markdown line —
 * the LSP deliberately declines to format prose positions, so a Tier 1
 * misfire there would never be corrected. (The pre-existing operator rules
 * are kept for chunk-editing parity; their prose leak predates the
 * assignment rule.)
 *
 * The parity tests gate drift between the two files on CI: everything except
 * the assignment rule must stay identical.
 */
const RMD_LANGUAGE_CONFIG = path.resolve(
    __dirname,
    '..',
    '..',
    'editors',
    'vscode',
    'rmd-language-configuration.json',
);

describe('rmd/quarto language configuration (assignment rule excluded)', () => {
    const rConfig = () => JSON.parse(fs.readFileSync(LANGUAGE_CONFIG, 'utf8'));
    const rmdConfig = () => JSON.parse(fs.readFileSync(RMD_LANGUAGE_CONFIG, 'utf8'));

    test('matches the R configuration outside onEnterRules', () => {
        const r = rConfig();
        const rmd = rmdConfig();
        delete r.onEnterRules;
        delete rmd.onEnterRules;
        expect(rmd).toEqual(r);
    });

    test('onEnterRules are exactly the R rules minus the assignment rule', () => {
        const rRules = rConfig().onEnterRules as OnEnterRule[];
        const expected = rRules.filter((rule) => !rule.beforeText.includes('<<-'));
        expect(expected.length).toBe(rRules.length - 1);
        expect(rmdConfig().onEnterRules).toEqual(expected);
    });

    test('package.json points r at the R config and rmd/quarto at the rmd config', () => {
        const pkg = JSON.parse(
            fs.readFileSync(
                path.resolve(__dirname, '..', '..', 'editors', 'vscode', 'package.json'),
                'utf8',
            ),
        );
        const byId = new Map(
            (pkg.contributes.languages as { id: string; configuration?: string }[]).map(
                (lang) => [lang.id, lang.configuration],
            ),
        );
        expect(byId.get('r')).toBe('./language-configuration.json');
        expect(byId.get('rmd')).toBe('./rmd-language-configuration.json');
        expect(byId.get('quarto')).toBe('./rmd-language-configuration.json');
    });
});
