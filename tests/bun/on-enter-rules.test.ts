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
        '`a\\`b` <-',
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
});
