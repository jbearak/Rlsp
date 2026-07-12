import * as vscode from 'vscode';
import {
    parseTree,
    visit,
    type Node,
    type ParseError,
} from 'jsonc-parser';

/**
 * Templates and commands that scaffold R-specific workspace files
 * (`.gitignore`) and Raven configuration (`.vscode/settings.json`).
 * Single-file writes prompt before overwriting; the linting-settings
 * scaffold merges into an existing settings file and only prompts when
 * it would overwrite existing project-scoped `raven.linting.*` keys.
 */

export const GITIGNORE_TEMPLATE = `# History files
.Rhistory
.Rapp.history

# Session Data files
.RData
.RDataTmp

# User-specific files
.Ruserdata

# RStudio files
.Rproj.user/

# R Environment Variables
.Renviron

# pkgdown site
docs/

# translation temp files
po/*~

# OS files
.DS_Store
Thumbs.db

# R Markdown / knitr artifacts
*_cache/
*_files/

# R CMD check output
.Rcheck/

# Quarto cache
.quarto/

# Local scratch / output
output/
scratch/
scratch.R

# AI tool user-local files
.claude/settings.local.json
.claude/agent-memory-local/
.claude/scheduled_tasks.lock
.cursorignore.local
`;

/**
 * Ordered groups of `raven.linting.*` keys. Each group renders as a
 * `// lintr: ...` heading followed by one or more key/value lines.
 * The leading-group `comment` is shown above the very first group as
 * the overview header. Keep the ordering stable so the file is easy
 * to diff after running the scaffold a second time.
 */
interface LintingGroup {
    comment: string;
    entries: Array<{ key: string; value: unknown }>;
}

const LINTING_SETTING_PREFIX = 'raven.linting.';

const CLIENT_ONLY_LINTING_SETTINGS = new Set(['raven.linting.readHomeLintr']);

export function isProjectScopedLintingSettingKey(key: string): boolean {
    return key.startsWith(LINTING_SETTING_PREFIX) && !CLIENT_ONLY_LINTING_SETTINGS.has(key);
}

const LINTING_GROUPS: LintingGroup[] = [
    {
        comment: 'Master switch — enable Raven\'s native style/lint diagnostics.',
        entries: [{ key: 'raven.linting.enabled', value: true }],
    },
    {
        comment: 'lintr: line_length_linter(length = N)',
        entries: [
            { key: 'raven.linting.lineLength', value: 120 },
            { key: 'raven.linting.lineLengthSeverity', value: 'information' },
        ],
    },
    {
        comment: 'lintr: trailing_whitespace_linter()',
        entries: [{ key: 'raven.linting.trailingWhitespaceSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: whitespace_linter() (no-tab portion)',
        entries: [{ key: 'raven.linting.noTabSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: trailing_blank_lines_linter()',
        entries: [{ key: 'raven.linting.trailingBlankLinesSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: assignment_linter()',
        entries: [
            { key: 'raven.linting.assignmentOperator', value: '<-' },
            { key: 'raven.linting.assignmentOperatorSeverity', value: 'information' },
        ],
    },
    {
        comment: 'lintr: object_name_linter(styles = ..., regexes = ...)',
        entries: [
            { key: 'raven.linting.objectNameStyleFunction', value: ['snake_case', 'symbols'] },
            { key: 'raven.linting.objectNameStyleVariable', value: ['snake_case', 'symbols'] },
            { key: 'raven.linting.objectNameStyleArgument', value: ['snake_case', 'symbols'] },
            { key: 'raven.linting.objectNameRegexesFunction', value: [] },
            { key: 'raven.linting.objectNameRegexesVariable', value: [] },
            { key: 'raven.linting.objectNameRegexesArgument', value: [] },
            { key: 'raven.linting.objectNameSeverity', value: 'information' },
        ],
    },
    {
        comment: 'lintr: infix_spaces_linter()',
        entries: [{ key: 'raven.linting.infixSpacesSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: commented_code_linter()',
        entries: [{ key: 'raven.linting.commentedCodeSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: quotes_linter() / single_quotes_linter()',
        entries: [
            { key: 'raven.linting.stringDelimiter', value: '"' },
            { key: 'raven.linting.quotesSeverity', value: 'information' },
        ],
    },
    {
        comment: 'lintr: commas_linter()',
        entries: [{ key: 'raven.linting.commasSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: T_and_F_symbol_linter()',
        entries: [{ key: 'raven.linting.tAndFSymbolSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: semicolon_linter()',
        entries: [{ key: 'raven.linting.semicolonSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: equals_na_linter()',
        entries: [{ key: 'raven.linting.equalsNaSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: object_length_linter(length = N)',
        entries: [
            { key: 'raven.linting.objectLength', value: 30 },
            { key: 'raven.linting.objectLengthSeverity', value: 'information' },
        ],
    },
    {
        comment: 'lintr: vector_logic_linter()',
        entries: [{ key: 'raven.linting.vectorLogicSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: function_left_parentheses_linter()',
        entries: [{ key: 'raven.linting.functionLeftParenthesesSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: spaces_inside_linter()',
        entries: [{ key: 'raven.linting.spacesInsideSeverity', value: 'information' }],
    },
    {
        comment: 'lintr: indentation_linter(indent = N)',
        entries: [
            { key: 'raven.linting.indentationUnit', value: 2 },
            { key: 'raven.linting.indentationSeverity', value: 'information' },
        ],
    },
    {
        comment: 'Raven-only, no lintr equivalent: infix continuation style',
        entries: [{ key: 'raven.linting.infixContinuationStyle', value: 'either' }],
    },
];

/**
 * The linting settings whose values are arrays, derived from
 * `LINTING_GROUPS` so a new array-valued scaffold entry is automatically
 * accepted by `classifyExisting` instead of requiring a second list to be
 * kept in sync by hand.
 */
const ARRAY_LINTING_SETTINGS = new Set(
    LINTING_GROUPS.flatMap((group) => group.entries)
        .filter((entry) => Array.isArray(entry.value))
        .map((entry) => entry.key),
);

const LINTING_BLOCK_HEADER =
    'Raven native style/lint diagnostics. Severities accept: "error",\n' +
    '"warning", "information", "hint", or "off". Each group below names\n' +
    'the lintr linter it mirrors (or is marked Raven-only). See\n' +
    'docs/linting.md for details.';

/**
 * Sentinel markers that delimit the block this scaffold manages. A
 * re-run of the scaffold strips the full sentinel range (including the
 * per-group `// lintr: ...` headers) before inserting a fresh block, so
 * a previously scaffolded file stays clean instead of accumulating
 * orphaned header comments. The markers are intentionally long and
 * stable so an accidental match against a user-authored comment is
 * unlikely.
 */
export const LINTING_SENTINEL_BEGIN =
    '// >>> raven.linting.* (managed by "Raven: Create linting settings")';
export const LINTING_SENTINEL_END = '// <<< raven.linting.*';

/**
 * Format the linting-settings block (sentinels + header + groups) at
 * the given indentation, without surrounding braces. Every entry — even
 * the last one — emits a trailing comma; the sentinel-end comment line
 * (and possibly more existing keys after our block in the merge case)
 * follows, so the comma is always correct in JSONC.
 */
function formatLintingBlock(indent: string): string {
    const lines: string[] = [];
    lines.push(`${indent}${LINTING_SENTINEL_BEGIN}`);
    for (const headerLine of LINTING_BLOCK_HEADER.split('\n')) {
        lines.push(`${indent}// ${headerLine}`);
    }

    for (const group of LINTING_GROUPS) {
        lines.push('');
        lines.push(`${indent}// ${group.comment}`);
        for (const entry of group.entries) {
            lines.push(
                `${indent}${JSON.stringify(entry.key)}: ${JSON.stringify(entry.value)},`,
            );
        }
    }

    lines.push(`${indent}${LINTING_SENTINEL_END}`);
    return lines.join('\n');
}

/**
 * The literal contents of a fresh `.vscode/settings.json` containing
 * just the linting block. Exported for unit tests; the production path
 * builds this via `buildLintingSettingsContent` so an existing file is
 * merged rather than clobbered.
 */
export const LINTING_SETTINGS_TEMPLATE = `{\n${formatLintingBlock('  ')}\n}\n`;

/**
 * Result of classifying the existing `.vscode/settings.json` for the
 * scaffold's merge step:
 *   - `empty`: file missing or whitespace-only — caller uses the fresh template.
 *   - `parseError`: text contains JSONC parse errors (including unterminated
 *     comments). `jsonc-parser` reports any error it finds; we surface them
 *     all the same way and refuse to merge.
 *   - `nonObjectRoot`: parsed fine but root isn't a JSON object (e.g. an
 *     array, scalar, or `null`). Can't safely merge into it.
 *   - `unsupportedValue`: a top-level project-scoped `raven.linting.*` key has
 *     a structured value Raven settings don't use (an object, or an array
 *     containing objects/arrays). Project-scoped `raven.linting.*` settings are
 *     scalars or arrays of scalars; deeper structures are likely user mistakes.
 *   - `object`: parsed as an object. `userManagedKeys` lists the top-level
 *     project-scoped `raven.linting.*` keys whose values are supported — these
 *     are the keys the scaffold prompts about before overwriting.
 */
type LintingClassification =
    | { kind: 'empty' }
    | { kind: 'parseError' }
    | { kind: 'nonObjectRoot' }
    | { kind: 'unsupportedValue'; key: string }
    | { kind: 'object'; userManagedKeys: string[] };

/**
 * Find the line indices of our sentinel-begin / sentinel-end markers.
 * Uses jsonc-parser's `visit` so a sentinel-shaped substring inside a
 * `/* ... *\/` block comment or string literal is correctly ignored:
 * `onComment` fires once per *whole* comment, so embedded `//` text
 * inside a block comment is not separately reported. Returns `null` if
 * either marker is missing or out of order.
 */
function findSentinelLineRange(text: string): { begin: number; end: number } | null {
    let beginLine: number | null = null;
    let beginOffset = -1;
    let endLine: number | null = null;

    visit(
        text,
        {
            onComment: (offset, length, startLine) => {
                // Restrict to `//` line comments — `/* ... */` block comments
                // arrive whole, and our sentinels are line-shaped.
                if (!text.startsWith('//', offset)) return;
                const commentText = text.slice(offset, offset + length).trim();
                if (commentText === LINTING_SENTINEL_BEGIN && beginLine === null) {
                    beginLine = startLine;
                    beginOffset = offset;
                } else if (
                    commentText === LINTING_SENTINEL_END &&
                    beginLine !== null &&
                    endLine === null &&
                    offset > beginOffset
                ) {
                    endLine = startLine;
                }
            },
        },
        // VS Code settings.json conventionally allows trailing commas; tell
        // the visitor so a trailing-comma error doesn't suppress later
        // `onComment` callbacks via jsonc-parser's error-recovery path.
        { allowTrailingComma: true },
    );

    if (beginLine === null || endLine === null) return null;
    return { begin: beginLine, end: endLine };
}

/**
 * Strip our sentinel-delimited block (sentinels + per-group `// lintr: ...`
 * headers + keys) out of `text`. Leaves everything else — including any
 * user-authored comments outside the sentinel range — untouched. No-op
 * when either sentinel is missing.
 */
function stripSentineledLintingBlock(text: string): string {
    const range = findSentinelLineRange(text);
    if (!range) return text;
    const lines = text.split('\n');
    lines.splice(range.begin, range.end - range.begin + 1);
    return lines.join('\n');
}

/**
 * Iterate the top-level properties of a parsed `jsonc-parser` object node,
 * yielding `{ key, valueNode }` pairs for project-scoped `raven.linting.*`
 * keys only. Client-only linting settings are preserved by the scaffold.
 * Skips malformed property nodes defensively.
 */
function* iterateLintingProperties(
    root: Node,
): Generator<{ key: string; valueNode: Node }> {
    if (root.type !== 'object' || !root.children) return;
    for (const prop of root.children) {
        if (prop.type !== 'property' || !prop.children || prop.children.length < 2) continue;
        const keyNode = prop.children[0];
        const valueNode = prop.children[1];
        if (typeof keyNode.value !== 'string') continue;
        if (!isProjectScopedLintingSettingKey(keyNode.value)) continue;
        yield { key: keyNode.value, valueNode };
    }
}

/**
 * Classify `text` for the scaffold's merge step. Wraps `jsonc-parser`'s
 * `parseTree` and adds the project-specific checks (non-object root,
 * unsupported structured project-scoped `raven.linting.*` value).
 */
function classifyExisting(text: string): LintingClassification {
    if (text.trim().length === 0) return { kind: 'empty' };
    const errors: ParseError[] = [];
    const root = parseTree(text, errors, { allowTrailingComma: true });
    if (errors.length > 0) return { kind: 'parseError' };
    if (!root || root.type !== 'object') return { kind: 'nonObjectRoot' };
    const userManagedKeys: string[] = [];
    for (const { key, valueNode } of iterateLintingProperties(root)) {
        if (!isSupportedLintingValueNode(key, valueNode)) {
            return { kind: 'unsupportedValue', key };
        }
        userManagedKeys.push(key);
    }
    return { kind: 'object', userManagedKeys };
}

function isSupportedLintingValueNode(key: string, valueNode: Node): boolean {
    if (valueNode.type === 'object') return false;
    if (valueNode.type !== 'array') return true;
    if (!ARRAY_LINTING_SETTINGS.has(key)) return false;
    return (valueNode.children ?? []).every(
        (child) => child.type !== 'object' && child.type !== 'array',
    );
}

/**
 * Same as `classifyExisting`, but applied to the post-sentinel-strip text
 * so callers only see *user-managed* project-scoped `raven.linting.*` keys
 * (everything inside our sentinel block was our own and is silently
 * regenerable).
 */
export function classifyUserManagedLintingKeys(text: string): LintingClassification {
    return classifyExisting(stripSentineledLintingBlock(text));
}

/**
 * The shape kept around for the test suite as a parse-success signal:
 * top-level project-scoped `raven.linting.*` keys, or `null` if classification
 * rejected the input for any reason. An empty file is a parse-success with
 * zero keys (not a rejection).
 */
export function detectExistingLintingKeys(text: string): string[] | null {
    const result = classifyExisting(text);
    if (result.kind === 'empty') return [];
    if (result.kind === 'object') return result.userManagedKeys;
    return null;
}

/**
 * Same as `detectExistingLintingKeys`, but ignores keys inside our sentinel-
 * managed block. A re-run of the scaffold should only prompt the user about
 * keys they wrote themselves (outside the sentinels), not ones a prior
 * scaffold run put there.
 */
export function detectUserManagedLintingKeys(text: string): string[] | null {
    const result = classifyUserManagedLintingKeys(text);
    if (result.kind === 'empty') return [];
    if (result.kind === 'object') return result.userManagedKeys;
    return null;
}

/**
 * Remove every top-level project-scoped `raven.linting.*` key from `text`.
 * Uses `jsonc-parser`'s `parseTree` to identify each key's property node (so
 * nested keys under e.g. a `[r]` language override and client-only linting
 * settings are left untouched), then splices the property's own range out of
 * the text.
 *
 * Why not `modify` + `applyEdits`? `jsonc-parser`'s `modify`-for-removal
 * has two edge-case bugs we hit:
 *   1. If the input has a trailing comma after the key being removed
 *      (`{ "raven.linting.x": 1, }`), `modify` produces `{ , }` —
 *      orphan comma, invalid JSONC.
 *   2. If a neighbour key has an inline `//` comment on the SAME
 *      physical line *before* our key (`"editor.tabSize": 4, // keep
 *      me\n"raven.linting.x": false`), `modify` consumes the comment
 *      as trailing content of the removed range and silently drops it.
 *
 * The splice removes the property + an optional trailing `,` + any
 * surrounding whitespace, plus the leading whitespace on the property's
 * line and the trailing newline *if* the property is alone on its line.
 * When the property shares a line with `{`, `}`, or other keys, we
 * leave the surrounding line structure intact and just take out our
 * key's range.
 */
function removeTopLevelLintingKeys(text: string): string {
    let current = text;
    // Hard cap matches the schema's `raven.linting.*` key count.
    for (let safety = 0; safety < 200; safety++) {
        const root = parseTree(current, undefined, { allowTrailingComma: true });
        if (!root || root.type !== 'object' || !root.children) break;

        let propNode: Node | null = null;
        for (const prop of root.children) {
            if (prop.type !== 'property' || !prop.children || prop.children.length < 2) {
                continue;
            }
            const key = prop.children[0].value;
            if (typeof key === 'string' && isProjectScopedLintingSettingKey(key)) {
                propNode = prop;
                break;
            }
        }
        if (!propNode) break;

        let removeStart = propNode.offset;
        let removeEnd = propNode.offset + propNode.length;

        // Phase 1: try to absorb a trailing `,` (standard "comma-after"
        // style) — skipping intermediate whitespace, tabs, and inline
        // `/* ... */` block comments (`"key": [..] /* note */,` must not
        // leave the comment + comma orphaned as invalid JSONC).
        let trailingCommaAbsorbed = false;
        {
            let j = removeEnd;
            for (;;) {
                while (j < current.length && (current[j] === ' ' || current[j] === '\t')) j++;
                if (current[j] === '/' && current[j + 1] === '*') {
                    const close = current.indexOf('*/', j + 2);
                    if (close === -1) break;
                    j = close + 2;
                    continue;
                }
                break;
            }
            if (current[j] === ',') {
                removeEnd = j + 1;
                trailingCommaAbsorbed = true;
            }
        }

        // Phase 2: if there was no trailing `,` to absorb, the file might
        // be using comma-before style (`"a": 1\n  , "raven.linting.x": v`).
        // Walk backward over whitespace looking for a leading `,` and
        // absorb it too — otherwise we'd leave an orphan separator.
        if (!trailingCommaAbsorbed) {
            let i = removeStart - 1;
            while (i >= 0 && (current[i] === ' ' || current[i] === '\t')) i--;
            if (i >= 0 && current[i] === ',') {
                removeStart = i;
            }
        }

        // Phase 3: if the (possibly-extended) removal range is alone on
        // its physical line, also take the leading whitespace, the
        // preceding newline, and a trailing newline so the line vanishes
        // cleanly. When the range shares a line with `{`, `}`, or a
        // neighbour key, we leave the surrounding line structure alone.
        let i = removeStart - 1;
        while (i >= 0 && (current[i] === ' ' || current[i] === '\t')) i--;
        const ownLine = i < 0 || current[i] === '\n';
        if (ownLine) {
            if (i >= 0) removeStart = i + 1;
            let j = removeEnd;
            while (j < current.length && (current[j] === ' ' || current[j] === '\t')) j++;
            if (current[j] === '\n') j++;
            removeEnd = j;
        }

        current = current.slice(0, removeStart) + current.slice(removeEnd);
    }
    return current;
}

/**
 * Walk `text[after..before)` skipping JSONC whitespace and comments,
 * and return `true` if the first significant character is `,`. Used to
 * decide whether the last property in a root object already has a
 * trailing comma (in which case we don't add another one).
 *
 * A block comment that opens inside the scan window but whose `*\/`
 * lies at or past `before` can't happen for valid JSONC (a `/* ... *\/`
 * straddling the root's closing brace would make the file unparseable
 * and we'd have bailed already in `classifyExisting`). The defensive
 * `if (!closed) return false;` branch below treats that case as "no
 * comma" anyway — the worst we'd do is insert a redundant comma.
 */
function hasCommaBetween(text: string, after: number, before: number): boolean {
    let i = after;
    while (i < before) {
        const c = text[i];
        if (c === ',') return true;
        if (c === ' ' || c === '\t' || c === '\n' || c === '\r') {
            i++;
            continue;
        }
        if (c === '/' && text[i + 1] === '/') {
            while (i < before && text[i] !== '\n') i++;
            continue;
        }
        if (c === '/' && text[i + 1] === '*') {
            i += 2;
            let closed = false;
            while (i + 1 < before) {
                if (text[i] === '*' && text[i + 1] === '/') {
                    closed = true;
                    break;
                }
                i++;
            }
            if (!closed) return false;
            i += 2;
            continue;
        }
        // Any other significant char shouldn't appear in valid JSONC
        // between a property value and the closing brace — treat as
        // "no comma" and let the caller deal with it.
        return false;
    }
    return false;
}

/**
 * Build the JSONC content to write to `.vscode/settings.json`. If
 * `existing` is `undefined` or whitespace-only, returns the fresh
 * template. Otherwise:
 *
 *   1. Strip any prior sentinel-managed block we wrote.
 *   2. Classify the rest via `jsonc-parser` (parse errors / non-object
 *      root / unsupported structured project-scoped `raven.linting.*` value
 *      all return `null`).
 *   3. Remove every remaining top-level project-scoped `raven.linting.*` key
 *      via `removeTopLevelLintingKeys`'s `parseTree`-driven line splice —
 *      `jsonc-parser`'s `modify` + `applyEdits` looked tempting here
 *      but has two edge-case bugs we hit (see that function's doc
 *      comment for why we don't use it).
 *   4. Use `parseTree` to find the closing `}` and the last property's
 *      value-end, then splice the formatted block in.
 *
 * Comma handling for the last existing property uses a small
 * comment-aware scan via `hasCommaBetween`: if the last property
 * already has a trailing comma we leave it, otherwise we insert one
 * right after the value (before any inline `//` comment on that line),
 * which is what `jsonc-parser.modify` would do for a fresh insertion
 * except `modify` also migrates the inline comment onto the new key's
 * line — surprising UX we'd rather avoid.
 */
export function buildLintingSettingsContent(existing: string | undefined): string | null {
    if (existing === undefined || existing.trim().length === 0) {
        return LINTING_SETTINGS_TEMPLATE;
    }

    const afterSentinelStrip = stripSentineledLintingBlock(existing);
    const classification = classifyExisting(afterSentinelStrip);
    if (classification.kind === 'empty') return LINTING_SETTINGS_TEMPLATE;
    if (classification.kind !== 'object') return null;

    const withoutLintingKeys = removeTopLevelLintingKeys(afterSentinelStrip);

    const root = parseTree(withoutLintingKeys, undefined, { allowTrailingComma: true });
    if (!root || root.type !== 'object') return null;

    // `parseTree` reports `root.offset` = position of `{` and
    // `root.length` spans through the closing `}` — so the `}` itself
    // sits at `root.offset + root.length - 1`.
    const closingBracePos = root.offset + root.length - 1;
    if (withoutLintingKeys[closingBracePos] !== '}') return null;

    const hasChildren = (root.children?.length ?? 0) > 0;

    let beforeInsertion: string;
    if (!hasChildren) {
        beforeInsertion = withoutLintingKeys.slice(0, closingBracePos);
    } else {
        const lastChild = root.children![root.children!.length - 1];
        const lastChildEnd = lastChild.offset + lastChild.length;
        const alreadyHasComma = hasCommaBetween(
            withoutLintingKeys,
            lastChildEnd,
            closingBracePos,
        );
        beforeInsertion = alreadyHasComma
            ? withoutLintingKeys.slice(0, closingBracePos)
            : withoutLintingKeys.slice(0, lastChildEnd) +
              ',' +
              withoutLintingKeys.slice(lastChildEnd, closingBracePos);
    }

    const trimmedBefore = beforeInsertion.replace(/[ \t\n\r]+$/, '');
    const after = withoutLintingKeys.slice(closingBracePos);
    const merged = `${trimmedBefore}\n${formatLintingBlock('  ')}\n${after}`;

    // Final safety net: the splice-based removal above handles the comment
    // and comma layouts we know about, but a layout it mishandles must never
    // be written back as invalid JSONC. Refuse to merge instead.
    const verifyErrors: ParseError[] = [];
    parseTree(merged, verifyErrors, { allowTrailingComma: true });
    if (verifyErrors.length > 0) return null;
    return merged;
}

export function renderRavenToml(linting: Record<string, unknown> | undefined): string {
    const lines: string[] = ['# Generated by Raven: Create raven.toml', ''];
    lines.push('[linting]');
    // Keep this list exhaustive across the `raven.linting.*` keys that have
    // portable `raven.toml` equivalents, excluding client-only discovery or
    // environment signals. Missing a severity here would create behavior drift
    // for CLI / non-VS-Code consumers, where `raven.toml` is the shared source
    // of truth. Any new lint rule should add both its value key (if it has one)
    // and its severity key here at the time the rule is added.
    const severities: [string, string][] = [
        ['lineLengthSeverity', 'information'],
        ['trailingWhitespaceSeverity', 'information'],
        ['noTabSeverity', 'information'],
        ['trailingBlankLinesSeverity', 'information'],
        ['assignmentOperatorSeverity', 'information'],
        ['objectNameSeverity', 'information'],
        ['infixSpacesSeverity', 'information'],
        ['commentedCodeSeverity', 'information'],
        ['quotesSeverity', 'information'],
        ['commasSeverity', 'information'],
        ['tAndFSymbolSeverity', 'information'],
        ['semicolonSeverity', 'information'],
        ['equalsNaSeverity', 'information'],
        ['objectLengthSeverity', 'information'],
        ['vectorLogicSeverity', 'information'],
        ['functionLeftParenthesesSeverity', 'information'],
        ['spacesInsideSeverity', 'information'],
        ['indentationSeverity', 'information'],
    ];
    const entries: [string, unknown, string][] = [
        ['enabled', false, 'master switch'],
        ['lineLength', 80, 'maximum line length (characters)'],
        ['objectLength', 30, 'maximum identifier length'],
        ['indentationUnit', 2, 'expected indent unit'],
        ['infixContinuationStyle', 'either', '"indented" | "aligned" | "either" (Raven-only)'],
        ['assignmentOperator', '<-', '"<-" or "="'],
        ['stringDelimiter', '"', '"\\"" or "\'"'],
        [
            'objectNameStyleFunction',
            ['snake_case', 'symbols'],
            'string or array: snake_case, camelCase, dotted.case, UPPER_CASE, lowercase, symbols, any',
        ],
        ['objectNameStyleVariable', ['snake_case', 'symbols'], 'same string-or-array form'],
        ['objectNameStyleArgument', ['snake_case', 'symbols'], 'same string-or-array form'],
        ['objectNameRegexesFunction', [], 'array of Rust regexes; partial match'],
        ['objectNameRegexesVariable', [], 'array of Rust regexes; partial match'],
        ['objectNameRegexesArgument', [], 'array of Rust regexes; partial match'],
        ...severities.map<[string, unknown, string]>(([k, d]) => [
            k,
            d,
            'error | warning | information | hint | off',
        ]),
    ];
    const ravenDefaultEnabled = false; // mirror VS Code package.json default
    for (const [key, dflt, comment] of entries) {
        const fromUser = linting?.[key];
        // `enabled` is special: the init-options factory always emits it
        // (see getInitializationOptions in initializationOptions.ts), so the "is this explicit?"
        // heuristic above doesn't apply. Treat enabled as explicit only when
        // it differs from the package.json default.
        const isExplicit =
            key === 'enabled'
                ? fromUser !== undefined && fromUser !== ravenDefaultEnabled
                : fromUser !== undefined;
        const value = isExplicit ? fromUser : dflt;
        const prefix = isExplicit ? '' : '# ';
        lines.push(`${prefix}${key} = ${toTomlScalar(value)}    # ${comment}`);
    }
    lines.push('');
    return lines.join('\n');
}

function toTomlScalar(v: unknown): string {
    if (typeof v === 'boolean' || typeof v === 'number') return String(v);
    return JSON.stringify(v);
}

/**
 * Return the first workspace folder, or surface a message and return
 * `undefined` if none is open. Without a workspace folder there is no
 * unambiguous place to write the scaffold file.
 */
function getTargetWorkspaceFolder(): vscode.WorkspaceFolder | undefined {
    const folders = vscode.workspace.workspaceFolders;
    if (!folders || folders.length === 0) {
        void vscode.window.showErrorMessage(
            'Raven: open a folder before creating an R scaffold file.',
        );
        return undefined;
    }
    return folders[0];
}

/**
 * Write `content` to `fileName` in the given workspace folder, prompting
 * before overwriting an existing file. Returns the target URI on success.
 */
export async function createScaffoldFile(
    folder: vscode.WorkspaceFolder,
    fileName: string,
    content: string,
): Promise<vscode.Uri | undefined> {
    const target = vscode.Uri.joinPath(folder.uri, fileName);

    let exists = false;
    try {
        await vscode.workspace.fs.stat(target);
        exists = true;
    } catch {
        exists = false;
    }

    if (exists) {
        const choice = await vscode.window.showWarningMessage(
            `${fileName} already exists in ${folder.name}. Overwrite?`,
            { modal: true },
            'Overwrite',
        );
        if (choice !== 'Overwrite') {
            return undefined;
        }
    }

    const bytes = Buffer.from(content, 'utf8');
    await vscode.workspace.fs.writeFile(target, bytes);

    const doc = await vscode.workspace.openTextDocument(target);
    await vscode.window.showTextDocument(doc, { preview: false });

    void vscode.window.setStatusBarMessage(
        `Raven: ${exists ? 'overwrote' : 'created'} ${fileName}`,
        3000,
    );

    return target;
}

/**
 * Run `createScaffoldFile` and surface a Raven-branded error notification on
 * failure. Mirrors the try/catch pattern used by `raven.refreshPackages` so
 * filesystem errors (permission denied, read-only workspace) get a clearer
 * message than VS Code's default rejection toast.
 */
async function runScaffoldCommand(fileName: string, content: string): Promise<void> {
    const folder = getTargetWorkspaceFolder();
    if (!folder) return;
    try {
        await createScaffoldFile(folder, fileName, content);
    } catch (err) {
        void vscode.window.showErrorMessage(
            `Raven: failed to create ${fileName}: ${err instanceof Error ? err.message : String(err)}`,
        );
    }
}

/**
 * Merge a Raven linting-settings block into `.vscode/settings.json`,
 * creating the file (and the `.vscode/` directory) if absent. If the
 * file already contains any project-scoped `raven.linting.*` keys, prompt
 * before overwriting them; unrelated keys and comments are preserved.
 */
async function runLintingSettingsScaffold(
    folder: vscode.WorkspaceFolder,
): Promise<vscode.Uri | undefined> {
    const vscodeDir = vscode.Uri.joinPath(folder.uri, '.vscode');
    const settingsUri = vscode.Uri.joinPath(vscodeDir, 'settings.json');
    const displayName = '.vscode/settings.json';

    let existing: string | undefined;
    try {
        const bytes = await vscode.workspace.fs.readFile(settingsUri);
        existing = Buffer.from(bytes).toString('utf8');
    } catch {
        existing = undefined;
    }

    if (existing !== undefined) {
        // Keys inside our sentinel-managed block were produced by an
        // earlier run of this same scaffold, so it's safe to regenerate
        // them silently. Only user-authored project-scoped `raven.linting.*`
        // keys (anything outside the sentinel range) trigger the prompt.
        const classification = classifyUserManagedLintingKeys(existing);
        if (classification.kind === 'parseError') {
            void vscode.window.showErrorMessage(
                `Raven: ${displayName} has JSON parse errors; fix them and re-run this command.`,
            );
            return undefined;
        }
        if (classification.kind === 'nonObjectRoot') {
            void vscode.window.showErrorMessage(
                `Raven: ${displayName} is valid JSON but its root isn't a JSON object — refusing to overwrite. Move the file aside (or wrap its contents in \`{}\`) and re-run this command.`,
            );
            return undefined;
        }
        if (classification.kind === 'unsupportedValue') {
            void vscode.window.showErrorMessage(
                `Raven: ${displayName} sets ${classification.key} to an unsupported structured value. Project-scoped raven.linting.* settings must be scalars or arrays of scalars; please correct the value before re-running this command.`,
            );
            return undefined;
        }
        const userManagedKeys =
            classification.kind === 'object' ? classification.userManagedKeys : [];
        if (userManagedKeys.length > 0) {
            const label =
                userManagedKeys.length === 1
                    ? '1 project-scoped raven.linting.* setting'
                    : `${userManagedKeys.length} project-scoped raven.linting.* settings`;
            const choice = await vscode.window.showWarningMessage(
                `${label} already in ${displayName}. Overwrite the project-scoped raven.linting.* block? Other keys and comments will be preserved.`,
                { modal: true },
                'Overwrite',
            );
            if (choice !== 'Overwrite') {
                return undefined;
            }
        }
    }

    const newContent = buildLintingSettingsContent(existing);
    if (newContent === null) {
        // Classification above should have caught this — defensive only.
        void vscode.window.showErrorMessage(
            `Raven: could not safely merge into ${displayName}.`,
        );
        return undefined;
    }

    try {
        await vscode.workspace.fs.createDirectory(vscodeDir);
    } catch {
        // directory may already exist; createDirectory is best-effort
    }

    await vscode.workspace.fs.writeFile(settingsUri, Buffer.from(newContent, 'utf8'));

    const doc = await vscode.workspace.openTextDocument(settingsUri);
    await vscode.window.showTextDocument(doc, { preview: false });

    void vscode.window.setStatusBarMessage(
        `Raven: ${existing === undefined ? 'created' : 'updated'} ${displayName}`,
        3000,
    );

    return settingsUri;
}

export function registerScaffoldCommands(context: vscode.ExtensionContext): void {
    context.subscriptions.push(
        vscode.commands.registerCommand('raven.scaffold.gitignore', () =>
            runScaffoldCommand('.gitignore', GITIGNORE_TEMPLATE),
        ),
    );

    context.subscriptions.push(
        vscode.commands.registerCommand('raven.scaffold.lintingSettings', async () => {
            const folder = getTargetWorkspaceFolder();
            if (!folder) return;
            try {
                await runLintingSettingsScaffold(folder);
            } catch (err) {
                void vscode.window.showErrorMessage(
                    `Raven: failed to update .vscode/settings.json: ${err instanceof Error ? err.message : String(err)}`,
                );
            }
        }),
    );
}
