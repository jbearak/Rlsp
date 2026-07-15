import { describe, test, expect } from 'bun:test';
import {
    classify_chunk_document,
    classify_chunk_document_for_document,
    detect_chunks,
    find_chunk_at_line,
    chunks_above,
    chunks_below,
    extract_chunk_code,
    has_chunk_anchor,
    is_runnable_chunk,
    next_runnable_chunk,
    previous_runnable_chunk,
    Chunk,
} from '../../editors/vscode/src/chunks/chunk-detector';

function lines(text: string): string[] {
    return text.split('\n');
}

describe('classify_chunk_document', () => {
    test('treats .Rmd and .Rmarkdown as rmd', () => {
        expect(classify_chunk_document('/tmp/foo.Rmd')).toBe('rmd');
        expect(classify_chunk_document('/tmp/foo.rmd')).toBe('rmd');
        expect(classify_chunk_document('/tmp/foo.Rmarkdown')).toBe('rmd');
        expect(classify_chunk_document('/tmp/foo.rmarkdown')).toBe('rmd');
        expect(classify_chunk_document('/tmp/foo.RMARKDOWN')).toBe('rmd');
    });

    test('treats .qmd as rmd', () => {
        expect(classify_chunk_document('/tmp/foo.qmd')).toBe('rmd');
        expect(classify_chunk_document('/tmp/foo.QMD')).toBe('rmd');
    });

    test('treats .R / .r as r', () => {
        expect(classify_chunk_document('/tmp/foo.R')).toBe('r');
        expect(classify_chunk_document('/tmp/foo.r')).toBe('r');
    });

    test('treats .md / .markdown as markdown', () => {
        expect(classify_chunk_document('/tmp/notes.md')).toBe('markdown');
        expect(classify_chunk_document('/tmp/notes.MD')).toBe('markdown');
        expect(classify_chunk_document('/tmp/notes.markdown')).toBe('markdown');
    });

    test('falls back to r for unknown extensions', () => {
        expect(classify_chunk_document('/tmp/foo.txt')).toBe('r');
    });
});

describe('detect_chunks — regular Markdown fenced blocks', () => {
    test('detects case-insensitive bare R fences', () => {
        const src = lines([
            '# Technical notes',
            '```R',
            'x <- 1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('r');
        expect(chunks[0].header_line).toBe(1);
        expect(chunks[0].end_line).toBe(2);
        expect(chunks[0].closing_fence_line).toBe(3);
        expect(chunks[0].kind).toBe('markdown');
    });

    test('detects tilde fences and up to three leading spaces', () => {
        const src = lines([
            '   ~~~r',
            '   x <- 1',
            ' y <- 2',
            '  ~~~',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('r');
        expect(chunks[0].closing_fence_line).toBe(3);
        expect(extract_chunk_code(src, chunks[0])).toBe('x <- 1\ny <- 2');
    });

    test('recognizes non-R fences but leaves them non-runnable', () => {
        const src = lines([
            '```python',
            'print("not R")',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('python');
        expect(is_runnable_chunk(chunks[0])).toBe(false);
    });

    test('does not treat knitr-style fences as regular Markdown R blocks', () => {
        const src = lines([
            '```{r}',
            'x <- 1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('{r}');
        expect(is_runnable_chunk(chunks[0])).toBe(false);
    });

    test('does not discover an R fence nested inside a longer non-R fence', () => {
        const src = lines([
            '````text',
            '```r',
            'x <- 1',
            '```',
            '````',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('text');
        expect(chunks[0].closing_fence_line).toBe(4);
    });

    test('does not discover an R fence nested inside an untagged outer fence', () => {
        const src = lines([
            '````',
            '```r',
            'x <- 1',
            '```',
            '````',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('');
        expect(chunks[0].closing_fence_line).toBe(4);
    });

    test('permits backticks in tilde-fence info strings without exposing nested R fences', () => {
        const src = lines([
            '~~~text`meta`',
            '```r',
            'x <- 1',
            '```',
            '~~~',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('text`meta`');
        expect(chunks[0].closing_fence_line).toBe(4);
    });

    test('rejects backticks anywhere in a backtick-fence info string', () => {
        const src = lines([
            '```r title=`demo`',
            'not part of a valid fenced block',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.every((chunk) => !is_runnable_chunk(chunk))).toBe(true);
    });

    test('ignores fenced-looking text inside multiline HTML comments', () => {
        const src = lines([
            '<!-- disabled example',
            '```r',
            'stop("must not run")',
            '```',
            '-->',
            '```r',
            'x <- 1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(5);
    });

    test('does not let inline comment syntax suppress a later fence', () => {
        const src = lines([
            'Document the literal `<!--` token here.',
            '',
            '```r',
            'x <- 1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(2);
    });

    test('ignores fenced-looking text inside raw HTML blocks', () => {
        const src = lines([
            '<pre>',
            '```r',
            'stop("must not run")',
            '```',
            '</pre>',
            '<div>',
            '```r',
            'also_not_runnable()',
            '```',
            '',
            '```r',
            'x <- 1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(10);
    });

    test('dedents tabs using CommonMark four-column tab stops', () => {
        const src = lines([
            '   ```r',
            '\tfoo',
            ' \tbar',
            '   ```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(extract_chunk_code(src, chunks[0])).toBe(' foo\n bar');
    });

    test('recognizes fences nested in Markdown containers', () => {
        const src = lines([
            '- example',
            '',
            '  ```r',
            '  x <- 1',
            '  ```',
            '',
            '> ```R',
            '> y <- 2',
            '> ```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(2);
        expect(chunks[0].header_line).toBe(2);
        expect(chunks[0].closing_fence_line).toBe(4);
        expect(extract_chunk_code(src, chunks[0])).toBe('x <- 1');
        expect(chunks[1].header_line).toBe(6);
        expect(chunks[1].closing_fence_line).toBe(8);
        expect(extract_chunk_code(src, chunks[1])).toBe('y <- 2');
    });

    test('ignores fenced-looking examples inside YAML front matter', () => {
        const src = lines([
            '---',
            'title: Technical notes',
            'example: |',
            '  ```r',
            '  stop("metadata only")',
            '  ```',
            '---',
            '',
            '```r',
            'x <- 1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'markdown');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(8);
        expect(extract_chunk_code(src, chunks[0])).toBe('x <- 1');
    });
});

describe('detect_chunks — Rmd/Qmd fenced blocks', () => {
    test('detects a single ```{r} chunk', () => {
        const src = lines([
            'Some prose.',
            '',
            '```{r}',
            'x <- 1',
            'print(x)',
            '```',
            '',
            'More prose.',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(2);
        expect(chunks[0].closing_fence_line).toBe(5);
        expect(chunks[0].end_line).toBe(4);
        expect(chunks[0].language).toBe('r');
        expect(chunks[0].is_eval_false).toBe(false);
    });

    test('parses header options including label and eval=FALSE', () => {
        const src = lines([
            '```{r setup, eval=FALSE, fig.width=4}',
            'x <- 1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('r');
        expect(chunks[0].label).toBe('setup');
        expect(chunks[0].options.eval).toBe('FALSE');
        expect(chunks[0].options['fig.width']).toBe('4');
        expect(chunks[0].is_eval_false).toBe(true);
    });

    test('keeps parenthesised vector values intact when splitting options', () => {
        const src = lines([
            '```{r, fig.dim=c(5, 6), out.width="80%"}',
            '1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks[0].options['fig.dim']).toBe('c(5, 6)');
        expect(chunks[0].options['out.width']).toBe('80%');
    });

    test('respects escaped quotes inside option values', () => {
        const src = lines([
            '```{r, lab="say \\"hi\\""}',
            '1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        // The escape stays in the parsed value — Raven does not interpret R string escapes.
        expect(chunks[0].options.lab).toBe('say \\"hi\\"');
    });

    test('recognizes eval = F as eval false', () => {
        const src = lines([
            '```{r, eval = F}',
            '1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks[0].is_eval_false).toBe(true);
    });

    test('recognizes upper-case R language tag', () => {
        const src = lines([
            '```{R}',
            '1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks[0].language).toBe('r');
    });

    test('detects non-R chunks (python, julia)', () => {
        const src = lines([
            '```{python}',
            'pass',
            '```',
            '',
            '```{julia}',
            '1',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks.length).toBe(2);
        expect(chunks[0].language).toBe('python');
        expect(chunks[1].language).toBe('julia');
    });

    test('detects multiple chunks separated by prose', () => {
        const src = lines([
            '```{r}',
            'a <- 1',
            '```',
            'Prose between.',
            '```{r second}',
            'b <- 2',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks.length).toBe(2);
        expect(chunks[0].header_line).toBe(0);
        expect(chunks[1].header_line).toBe(4);
        expect(chunks[1].label).toBe('second');
    });

    test('handles unclosed fence by extending to EOF', () => {
        const src = lines([
            '```{r}',
            'x <- 1',
            'y <- 2',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks.length).toBe(1);
        expect(chunks[0].closing_fence_line).toBeNull();
        expect(chunks[0].end_line).toBe(2);
    });

    test('handles tilde fences', () => {
        const src = lines([
            '~~~{r}',
            'x <- 1',
            '~~~',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks.length).toBe(1);
        expect(chunks[0].language).toBe('r');
    });

    test('handles fences with more backticks (4+)', () => {
        const src = lines([
            '````{r}',
            '```',
            'inner backticks',
            '```',
            '````',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks.length).toBe(1);
        expect(chunks[0].closing_fence_line).toBe(4);
    });

    test('ignores indented fences (Pandoc-style is line-start)', () => {
        // Two leading spaces — code-block indent in Pandoc treats this as a literal, not a fence.
        const src = lines([
            '  ```{r}',
            '  x <- 1',
            '  ```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        // We don't try to parse Pandoc semantics: just require the fence at column 0.
        expect(chunks.length).toBe(0);
    });

    test('returns empty for documents with no fences', () => {
        const src = lines(['# Title', 'Just prose.'].join('\n'));
        expect(detect_chunks(src, 'rmd').length).toBe(0);
    });

    test('treats a header inside an open chunk as content, not a new chunk', () => {
        // Outer ```` ```` fence wraps an inner ``` ``` ``` fence header
        // verbatim. We should see one outer chunk and no inner one — the
        // detector resumes scanning AFTER the outer closing fence.
        const src = lines([
            '````{r outer}',
            '```{r inner}',
            'x <- 1',
            '```',
            '````',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(chunks.length).toBe(1);
        expect(chunks[0].label).toBe('outer');
        expect(chunks[0].closing_fence_line).toBe(4);
    });
});

describe('detect_chunks — .R cell markers', () => {
    test('detects # %% cells', () => {
        const src = lines([
            '# initial code',
            'x <- 1',
            '# %% First cell',
            'a <- 1',
            'b <- 2',
            '# %% Second cell',
            'c <- 3',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(2);
        expect(chunks[0].header_line).toBe(2);
        expect(chunks[0].end_line).toBe(4);
        expect(chunks[0].language).toBe('r');
        expect(chunks[1].header_line).toBe(5);
        expect(chunks[1].end_line).toBe(6);
    });

    test('matches multiple-hash markers', () => {
        const src = lines([
            '### %% Heading-style',
            'x <- 1',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(0);
    });

    test('does not match `# %%%` or `# %%inline` non-markers', () => {
        const src = lines([
            '# %%%',
            'x <- 1',
            '# %%not-a-cell',
            'y <- 2',
            '# %% real cell',
            'z <- 3',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(4);
    });

    test('returns empty for .R with no markers', () => {
        const src = lines(['x <- 1', 'print(x)'].join('\n'));
        expect(detect_chunks(src, 'r').length).toBe(0);
    });

    test('empty cell (no following content) is still a chunk', () => {
        const src = lines(['# %%'].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(0);
        expect(chunks[0].end_line).toBe(0);
    });

    test('section divider line ends the current cell (cell-end only, not cell-start)', () => {
        const src = lines([
            '# %% one',
            'x <- 1',
            '# Section ====',
            'y <- 2',
            '# %% two',
            'z <- 3',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(2);
        // Cell 1 ends at the section divider line itself (line 2).
        expect(chunks[0].header_line).toBe(0);
        expect(chunks[0].end_line).toBe(2);
        // Cell 2 starts at its own # %% header. The orphan `y <- 2` line between
        // the section divider and `# %% two` is NOT part of any cell.
        expect(chunks[1].header_line).toBe(4);
        expect(chunks[1].end_line).toBe(5);
    });

    test('section divider with dashes ends the cell, orphan content between is excluded', () => {
        const src = lines([
            '# %% Load',
            'library(dplyr)',
            '# Setup ----',
            'helper <- 1',
            '# %% Transform',
            'x <- 1',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(2);
        expect(chunks[0].header_line).toBe(0);
        // Cell 1 ends at the divider on line 2; `helper <- 1` is orphan.
        expect(chunks[0].end_line).toBe(2);
        expect(chunks[1].header_line).toBe(4);
        expect(chunks[1].end_line).toBe(5);
    });

    test('section divider with hashes ends the cell, orphan content between is excluded', () => {
        const src = lines([
            '# %% First',
            'a <- 1',
            '## Section #####',
            'orphan <- 1',
            '# %% Second',
            'b <- 2',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(2);
        expect(chunks[0].end_line).toBe(2);
        expect(chunks[1].header_line).toBe(4);
        expect(chunks[1].end_line).toBe(5);
    });

    test('section divider before any # %% is not a chunk by itself', () => {
        const src = lines([
            '# Setup ----',
            'x <- 1',
            '# %% main',
            'y <- 2',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        // Section divider doesn't start a cell; the only cell is `# %% main`.
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(2);
        expect(chunks[0].end_line).toBe(3);
    });

    test('line that matches both # %% and section divider is treated as cell marker', () => {
        // `# %% ====` matches both regexes; cell marker takes priority so it
        // becomes a new cell header, not a cell-end of the prior cell.
        const src = lines([
            '# %% one',
            'x <- 1',
            '# %% ====',
            'y <- 2',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(2);
        expect(chunks[0].header_line).toBe(0);
        expect(chunks[0].end_line).toBe(1);
        expect(chunks[1].header_line).toBe(2);
        expect(chunks[1].end_line).toBe(3);
    });

    test('section divider requires at least 4 boundary characters', () => {
        // `# Title ===` has only 3 `=` — not a section divider.
        const src = lines([
            '# %% one',
            '# Title ===',
            'x <- 1',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(1);
        // The `# Title ===` line is part of cell 1, not a boundary.
        expect(chunks[0].end_line).toBe(2);
    });

    test('section divider on the last line ends the cell at EOF', () => {
        const src = lines([
            '# %% one',
            'x <- 1',
            '# End ====',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(1);
        expect(chunks[0].header_line).toBe(0);
        expect(chunks[0].end_line).toBe(2);
    });

    test("roxygen #' lines do not act as section dividers", () => {
        // Roxygen doc comments start with `#'` and sometimes use trailing
        // dashes for visual emphasis. They are NOT RStudio section dividers
        // and must not terminate the surrounding cell.
        const src = lines([
            '# %% docs',
            "#' @param x A value -----------",
            "#' @return numeric ============",
            'f <- function(x) x',
            '# %% next',
            'g <- 1',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(chunks.length).toBe(2);
        // Cell 1 spans from `# %% docs` (line 0) all the way to `f <- function(x) x` (line 3).
        expect(chunks[0].header_line).toBe(0);
        expect(chunks[0].end_line).toBe(3);
        expect(chunks[1].header_line).toBe(4);
    });
});

describe('find_chunk_at_line', () => {
    const chunks: Chunk[] = [
        { header_line: 2, end_line: 4, closing_fence_line: 5, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' },
        { header_line: 7, end_line: 9, closing_fence_line: 10, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' },
    ];

    test('returns chunk when cursor is inside body', () => {
        expect(find_chunk_at_line(chunks, 3)?.header_line).toBe(2);
        expect(find_chunk_at_line(chunks, 8)?.header_line).toBe(7);
    });

    test('returns chunk when cursor is on header line', () => {
        expect(find_chunk_at_line(chunks, 2)?.header_line).toBe(2);
    });

    test('returns chunk when cursor is on closing fence', () => {
        expect(find_chunk_at_line(chunks, 5)?.header_line).toBe(2);
    });

    test('returns null when cursor is outside any chunk', () => {
        expect(find_chunk_at_line(chunks, 0)).toBeNull();
        expect(find_chunk_at_line(chunks, 6)).toBeNull();
    });
});

describe('chunks_above', () => {
    const chunks: Chunk[] = [
        { header_line: 0, end_line: 2, closing_fence_line: 3, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' },
        { header_line: 5, end_line: 6, closing_fence_line: 7, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' },
        { header_line: 10, end_line: 12, closing_fence_line: 13, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' },
    ];

    test('returns chunks whose closing position is strictly above cursor', () => {
        // Cursor on line 8 (between chunk 2's close at 7 and chunk 3's header at 10)
        // → chunks 1 and 2 are above.
        const result = chunks_above(chunks, 8);
        expect(result.length).toBe(2);
    });

    test('when cursor is inside a chunk, chunks above does not include the current chunk', () => {
        // Cursor in chunk 2 (line 6) → only chunk 1 is above.
        const result = chunks_above(chunks, 6);
        expect(result.length).toBe(1);
        expect(result[0].header_line).toBe(0);
    });

    test('returns empty list when cursor is above all chunks', () => {
        const result = chunks_above(chunks, 0);
        expect(result.length).toBe(0);
    });
});

describe('chunks_below', () => {
    const chunks: Chunk[] = [
        { header_line: 0, end_line: 2, closing_fence_line: 3, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' },
        { header_line: 5, end_line: 6, closing_fence_line: 7, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' },
        { header_line: 10, end_line: 12, closing_fence_line: 13, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' },
    ];

    test('returns chunks whose header is strictly below cursor', () => {
        // Cursor on line 8 (between chunk 2's close at 7 and chunk 3's header at 10)
        // → only chunk 3 is below.
        const result = chunks_below(chunks, 8);
        expect(result.length).toBe(1);
        expect(result[0].header_line).toBe(10);
    });

    test('when cursor is on a header, that chunk is not below', () => {
        // Cursor on chunk 2's header at line 5 → only chunk 3 (header 10) is below.
        const result = chunks_below(chunks, 5);
        expect(result.length).toBe(1);
        expect(result[0].header_line).toBe(10);
    });

    test('returns empty list when cursor is past every chunk', () => {
        const result = chunks_below(chunks, 99);
        expect(result.length).toBe(0);
    });
});

describe('previous_runnable_chunk / next_runnable_chunk', () => {
    function r_chunk(header: number, last: number): Chunk {
        return {
            header_line: header,
            end_line: last,
            closing_fence_line: last + 1,
            language: 'r',
            label: null,
            options: {},
            is_eval_false: false,
            kind: 'rmd',
        };
    }
    function py_chunk(header: number, last: number): Chunk {
        return { ...r_chunk(header, last), language: 'python' };
    }

    test('previous returns the chunk before the one containing the cursor', () => {
        const chunks: Chunk[] = [r_chunk(0, 2), r_chunk(5, 7), r_chunk(10, 12)];
        // Cursor inside chunk 2 (line 6) → previous is chunk 1.
        expect(previous_runnable_chunk(chunks, 6)?.header_line).toBe(0);
    });

    test('next returns the chunk after the one containing the cursor', () => {
        const chunks: Chunk[] = [r_chunk(0, 2), r_chunk(5, 7), r_chunk(10, 12)];
        // Cursor inside chunk 2 (line 6) → next is chunk 3.
        expect(next_runnable_chunk(chunks, 6)?.header_line).toBe(10);
    });

    test('previous returns the nearest chunk strictly above when cursor is in prose', () => {
        const chunks: Chunk[] = [r_chunk(0, 2), r_chunk(5, 7), r_chunk(10, 12)];
        // Cursor on line 9 (between chunk 2 and 3) → previous is chunk 2.
        expect(previous_runnable_chunk(chunks, 9)?.header_line).toBe(5);
    });

    test('next returns the nearest chunk strictly below when cursor is in prose', () => {
        const chunks: Chunk[] = [r_chunk(0, 2), r_chunk(5, 7), r_chunk(10, 12)];
        // Cursor on line 9 → next is chunk 3.
        expect(next_runnable_chunk(chunks, 9)?.header_line).toBe(10);
    });

    test('previous skips non-runnable chunks', () => {
        // R (0..2), Python (5..7), R (10..12). Cursor inside the last R chunk
        // (line 11) → previous runnable is R(0) because the only "previous"
        // chunk between is Python and gets skipped.
        const chunks: Chunk[] = [r_chunk(0, 2), py_chunk(5, 7), r_chunk(10, 12)];
        expect(previous_runnable_chunk(chunks, 11)?.header_line).toBe(0);
    });

    test('next skips non-runnable chunks', () => {
        const chunks: Chunk[] = [r_chunk(0, 2), py_chunk(5, 7), r_chunk(10, 12)];
        // Cursor at line 3 → next runnable is the R chunk at line 10, skipping python.
        expect(next_runnable_chunk(chunks, 3)?.header_line).toBe(10);
    });

    test('returns null when no chunk above / below', () => {
        const chunks: Chunk[] = [r_chunk(5, 7)];
        expect(previous_runnable_chunk(chunks, 5)).toBeNull();
        expect(next_runnable_chunk(chunks, 7)).toBeNull();
    });
});

describe('extract_chunk_code', () => {
    test('extracts content between fence lines, excluding fences', () => {
        const src = lines([
            'prose',
            '```{r}',
            'x <- 1',
            'y <- 2',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(extract_chunk_code(src, chunks[0])).toBe('x <- 1\ny <- 2');
    });

    test('extracts content after cell marker', () => {
        const src = lines([
            '# %% one',
            'x <- 1',
            'y <- 2',
        ].join('\n'));
        const chunks = detect_chunks(src, 'r');
        expect(extract_chunk_code(src, chunks[0])).toBe('x <- 1\ny <- 2');
    });

    test('returns empty for an empty chunk body', () => {
        const src = lines([
            '```{r}',
            '```',
        ].join('\n'));
        const chunks = detect_chunks(src, 'rmd');
        expect(extract_chunk_code(src, chunks[0])).toBe('');
    });
});

describe('classify_chunk_document_for_document', () => {
    test('prefers languageId rmd / quarto over URI extension', () => {
        const doc = { languageId: 'rmd', uri: { fsPath: '/tmp/untitled-1', path: '/tmp/untitled-1' } };
        expect(classify_chunk_document_for_document(doc)).toBe('rmd');
        const qdoc = { languageId: 'Quarto', uri: { fsPath: '', path: 'Untitled-1' } };
        expect(classify_chunk_document_for_document(qdoc)).toBe('rmd');
    });

    test('prefers the Markdown languageId for untitled buffers', () => {
        const doc = { languageId: 'Markdown', uri: { fsPath: '', path: 'Untitled-1' } };
        expect(classify_chunk_document_for_document(doc)).toBe('markdown');
    });

    test('preserves Rmd/Qmd extension classification under a Markdown languageId', () => {
        const rmd = { languageId: 'markdown', uri: { fsPath: '/tmp/report.Rmd', path: '/tmp/report.Rmd' } };
        expect(classify_chunk_document_for_document(rmd)).toBe('rmd');
        const qmd = { languageId: 'markdown', uri: { fsPath: '/tmp/report.qmd', path: '/tmp/report.qmd' } };
        expect(classify_chunk_document_for_document(qmd)).toBe('rmd');
    });

    test('falls back to URI extension for `r` languageId', () => {
        const rmd = { languageId: 'r', uri: { fsPath: '/tmp/report.Rmd', path: '/tmp/report.Rmd' } };
        expect(classify_chunk_document_for_document(rmd)).toBe('rmd');
        const rmarkdown = { languageId: 'r', uri: { fsPath: '/tmp/report.Rmarkdown', path: '/tmp/report.Rmarkdown' } };
        expect(classify_chunk_document_for_document(rmarkdown)).toBe('rmd');
        const qmd = { languageId: 'r', uri: { fsPath: '/tmp/notes.qmd', path: '/tmp/notes.qmd' } };
        expect(classify_chunk_document_for_document(qmd)).toBe('rmd');
        const r = { languageId: 'r', uri: { fsPath: '/tmp/main.R', path: '/tmp/main.R' } };
        expect(classify_chunk_document_for_document(r)).toBe('r');
    });

    test('returns r for an untitled buffer with languageId r and no extension', () => {
        const doc = { languageId: 'r', uri: { fsPath: '', path: 'Untitled-1' } };
        expect(classify_chunk_document_for_document(doc)).toBe('r');
    });
});

describe('has_chunk_anchor', () => {
    test('returns false for plain R code with no markers', () => {
        const text = 'x <- 1\nprint(x)\n# comment\nfn <- function(a, b) a + b\n';
        expect(has_chunk_anchor(text, 'r')).toBe(false);
    });

    test('returns true when an .R file contains `%%`', () => {
        // Coarse: any `%%` in the text triggers the slow path. False positives
        // are acceptable; the real regex confirms whether the line is a marker.
        expect(has_chunk_anchor('# %% one\nx <- 1', 'r')).toBe(true);
        expect(has_chunk_anchor('x <- "50%%"\n', 'r')).toBe(true);
    });

    test('returns false for prose-only Rmd / Qmd', () => {
        const text = '# Title\n\nSome prose.\nNo fences here.\n';
        expect(has_chunk_anchor(text, 'rmd')).toBe(false);
    });

    test('returns true when an Rmd document contains backtick or tilde fences', () => {
        expect(has_chunk_anchor('```{r}\n1\n```\n', 'rmd')).toBe(true);
        expect(has_chunk_anchor('~~~{r}\n1\n~~~\n', 'rmd')).toBe(true);
    });

    test('returns true when a Markdown document contains a fence', () => {
        expect(has_chunk_anchor('```r\n1\n```\n', 'markdown')).toBe(true);
    });

    test('does not false-trigger on single backticks (inline code)', () => {
        const text = 'Inline `r 1 + 1` math here.\n';
        expect(has_chunk_anchor(text, 'rmd')).toBe(false);
    });
});

describe('is_runnable_chunk', () => {
    test('r chunks are runnable', () => {
        const c: Chunk = { header_line: 0, end_line: 1, closing_fence_line: 2, language: 'r', label: null, options: {}, is_eval_false: false, kind: 'rmd' };
        expect(is_runnable_chunk(c)).toBe(true);
    });

    test('non-r chunks are not runnable', () => {
        const c: Chunk = { header_line: 0, end_line: 1, closing_fence_line: 2, language: 'python', label: null, options: {}, is_eval_false: false, kind: 'rmd' };
        expect(is_runnable_chunk(c)).toBe(false);
    });
});
