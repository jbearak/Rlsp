//! Code-chunk detection for R Markdown / Quarto fenced blocks and `.R` cell markers.
//!
//! Rust port of `editors/vscode/src/chunks/chunk-detector.ts`. Detection is
//! text-based (no tree-sitter), so it works on documents whose body would
//! otherwise be opaque to the R parser (prose, YAML, fenced non-R code).
//!
//! Used by `SymbolExtractor::extract_chunks` to surface chunk entries in the
//! document outline. Kept aligned with the TypeScript detector so the two
//! continue to produce the same `header_line` / `end_line` for any document.
//! The module also provides [`mask_to_r`] for the geometry-preserving masked
//! analysis representation of Rmd/Quarto documents, which replaces all
//! non-R-body lines with empty strings while preserving line/column geometry.

use regex::Regex;
use std::sync::OnceLock;

/// Which detection path to use when scanning a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    /// Rmd/Qmd fenced block (` ```{r ...} ` … ` ``` `).
    Rmd,
    /// `.R` cell marker (`# %%`).
    R,
}

/// A detected chunk.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// 0-based line index of the chunk header (fence or `# %%` line).
    pub header_line: u32,
    /// 0-based line index of the last content line (inclusive). For an Rmd
    /// chunk this is one line above `closing_fence_line` when the fence is
    /// present, or the last line of the file when unclosed.
    pub end_line: u32,
    /// 0-based line index of the closing fence (Rmd only). `None` for `.R`
    /// cells and for unclosed Rmd chunks that run off the end of the file.
    pub closing_fence_line: Option<u32>,
    /// Language tag from the chunk header, lower-cased. `.R` cells are always
    /// `"r"`.
    pub language: String,
    /// First bare identifier in the header (e.g. `setup` in
    /// `{r setup, eval=FALSE}`), or — for `# %%` cells — the text after the
    /// marker. `None` when no label is present.
    pub label: Option<String>,
    /// `true` when the chunk header contains a literal `eval = FALSE` or
    /// `eval = F` option. Such chunks are display-only (never executed by
    /// knitr) and their body may contain intentionally malformed R — blanking
    /// them in [`mask_to_r`] prevents spurious syntax diagnostics.
    pub eval_disabled: bool,
}

fn fence_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(`{3,}|~{3,})\s*\{([A-Za-z0-9_+.\-]+)([^}]*)\}\s*$")
            .expect("fence header regex")
    })
}

fn fence_close_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(`{3,}|~{3,})\s*$").expect("fence close regex"))
}

fn cell_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `# %%` (any number of leading `#`), followed by EOL or whitespace.
    // Excludes `# %%%` (3+ `%`) and `# %%inline-text`.
    RE.get_or_init(|| Regex::new(r"^#+\s*%%(?:[^%\S\r\n].*|\s*)?$").expect("cell marker regex"))
}

fn section_divider_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // RStudio-style section divider: `# Title ====`, `# Setup ----`, etc.
    // Acts as a cell-END boundary when mixed with `# %%` cells.
    // Excludes roxygen `#'` lines.
    RE.get_or_init(|| {
        // `[^']` (mandatory, not `[^']?`) — without it, the character class
        // could match zero chars and a roxygen line like `#' Title ----`
        // would be misread as a divider.
        Regex::new(r"^#+[^'].*[-#+=*]{4,}\s*$").expect("section divider regex")
    })
}

/// Classify a document path (URI or file path) by extension. Falls back to
/// [`ChunkKind::R`] for unknown extensions so behavior is predictable.
pub fn classify_chunk_document(path_or_uri: &str) -> ChunkKind {
    let lower = path_or_uri.to_ascii_lowercase();
    if lower.ends_with(".rmd") || lower.ends_with(".rmarkdown") || lower.ends_with(".qmd") {
        ChunkKind::Rmd
    } else {
        ChunkKind::R
    }
}

/// Classify a document using its `languageId` first, then its URI path.
///
/// Matches the client-side `classify_chunk_document_for_document` helper in
/// `editors/vscode/src/chunks/chunk-detector.ts` so untitled buffers — which
/// have no file extension — still classify correctly when the editor passes
/// `languageId: "rmd" | "quarto"`.
pub fn classify_chunk_document_for(language_id: Option<&str>, path_or_uri: &str) -> ChunkKind {
    if let Some(lang) = language_id {
        match lang.to_ascii_lowercase().as_str() {
            "rmd" | "quarto" => return ChunkKind::Rmd,
            "r" => return classify_chunk_document(path_or_uri),
            _ => {}
        }
    }
    classify_chunk_document(path_or_uri)
}

/// Detect all chunks in the document, in source order. `kind` controls which
/// detection path runs.
pub fn detect_chunks(text: &str, kind: ChunkKind) -> Vec<Chunk> {
    // BOM-tolerant split: both paths anchor their fence/cell/divider regexes at
    // column 0 (see `lines_for_column0_scan`). Chunks report only line numbers. #346.
    let lines = crate::utf16::lines_for_column0_scan(text);
    match kind {
        ChunkKind::Rmd => detect_rmd_chunks(&lines),
        ChunkKind::R => detect_r_cells(&lines),
    }
}

fn detect_rmd_chunks(lines: &[&str]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let header_re = fence_header_re();
    let close_re = fence_close_re();

    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let Some(caps) = header_re.captures(line) else {
            i += 1;
            continue;
        };

        let fence = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let lang = caps
            .get(2)
            .map(|m| m.as_str().to_ascii_lowercase())
            .unwrap_or_default();
        let header_rest = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let label = parse_header_label(header_rest);
        let eval_disabled = has_eval_false(header_rest);

        let fence_char = fence.chars().next().unwrap_or('`');
        let min_len = fence.len();
        let mut closing_line: Option<u32> = None;
        for (j, &line) in lines.iter().enumerate().skip(i + 1) {
            if let Some(close_caps) = close_re.captures(line) {
                let close = close_caps.get(1).map(|m| m.as_str()).unwrap_or("");
                if close.starts_with(fence_char) && close.len() >= min_len {
                    closing_line = Some(j as u32);
                    break;
                }
            }
        }

        let header_line = i as u32;
        let end_line = match closing_line {
            Some(close) if close > 0 => close - 1,
            Some(_) => header_line,
            None => (lines.len().saturating_sub(1)) as u32,
        };
        let end_line = end_line.max(header_line);

        chunks.push(Chunk {
            header_line,
            end_line,
            closing_fence_line: closing_line,
            language: lang,
            label,
            eval_disabled,
        });

        i = match closing_line {
            Some(close) => (close as usize) + 1,
            None => lines.len(),
        };
    }
    chunks
}

fn detect_r_cells(lines: &[&str]) -> Vec<Chunk> {
    let marker_re = cell_marker_re();
    let divider_re = section_divider_re();

    // Pass 1: enumerate cell markers and section dividers.
    let mut markers: Vec<usize> = Vec::new();
    let mut dividers: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (idx, line) in lines.iter().enumerate() {
        if marker_re.is_match(line) {
            markers.push(idx);
        } else if divider_re.is_match(line) {
            dividers.insert(idx);
        }
    }

    // Pass 2: each marker is a cell that runs until the next marker, the next
    // section divider, or EOF — whichever comes first.
    let mut chunks = Vec::with_capacity(markers.len());
    for (m, &header) in markers.iter().enumerate() {
        let next_marker = markers.get(m + 1).copied().unwrap_or(lines.len());
        let mut end_line = next_marker.saturating_sub(1);
        for i in (header + 1)..next_marker {
            if dividers.contains(&i) {
                end_line = i;
                break;
            }
        }
        let header_line = header as u32;
        let end_line = (end_line as u32).max(header_line);
        chunks.push(Chunk {
            header_line,
            end_line,
            closing_fence_line: None,
            language: "r".to_string(),
            label: cell_label(lines[header]),
            eval_disabled: false,
        });
    }
    chunks
}

/// Extract the human-readable label from a `# %%` cell marker line.
/// Returns `None` when the marker has no trailing text.
fn cell_label(line: &str) -> Option<String> {
    // Trim leading `#`s and whitespace, drop the `%%` token, then return the
    // remainder (trimmed) as the label. Returns None when nothing remains.
    let mut rest = line.trim_start();
    while rest.starts_with('#') {
        rest = &rest[1..];
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("%%").unwrap_or(rest);
    let label = rest.trim().trim_end_matches(|c: char| {
        // Strip trailing decorative `-#+=*` runs (e.g. `# %% Setup ----`).
        matches!(c, '-' | '#' | '+' | '=' | '*' | ' ' | '\t')
    });
    let label = label.trim();
    if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    }
}

/// Parse the body of a chunk header (everything between `{lang` and `}`) and
/// return the optional label (first bare identifier).
///
/// Comma splitting respects nested brackets and quoted strings so that values
/// like `fig.dim=c(5, 6)` or `lab="a,b"` don't trip the parser.
fn parse_header_label(rest: &str) -> Option<String> {
    for raw in split_header_options(rest) {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        if part.contains('=') {
            // key=value option, not the label.
            continue;
        }
        return Some(part.to_string());
    }
    None
}

/// Returns `true` when the chunk header options contain a literal
/// `eval = FALSE` or `eval = F` (the only R expressions that disable
/// evaluation statically). Uses the same bracket/quote-aware comma split as
/// [`parse_header_label`] so nested commas (e.g. `fig.dim=c(5, 6)`) don't
/// confuse the scan.
fn has_eval_false(header_rest: &str) -> bool {
    for raw in split_header_options(header_rest) {
        let part = raw.trim();
        // Match `eval = FALSE`, `eval=F`, `eval = F`, `eval=FALSE`.
        if let Some(val) = part.strip_prefix("eval") {
            let val = val.trim_start();
            if let Some(val) = val.strip_prefix('=') {
                let val = val.trim();
                if val == "FALSE" || val == "F" {
                    return true;
                }
            }
        }
    }
    false
}

/// True for chunk language tags that should be parsed as R. Pandoc/knitr
/// permit a few aliases (`r`, `R`, plus the rare `Rscript`). The chunk
/// detector lower-cases the tag before storing it, so a simple ASCII compare
/// is sufficient here.
pub(crate) fn is_r_chunk_language(language: &str) -> bool {
    matches!(language, "r" | "rscript")
}

/// A compiled regex for the knitr chunk-reuse reference pattern `<<label>>`.
///
/// Lines matching `^\s*<<[^>]+>>\s*$` (optionally with a trailing `\r`) are
/// knitr meta-syntax, not R code, and must be blanked by [`mask_to_r`].
fn chunk_reuse_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*<<[^>]+>>\s*\r?$").expect("chunk reuse reference regex"))
}

/// Body-line range of an R chunk, clamped to the document.
///
/// Returns `None` when the chunk is not R, has no body, or starts past EOF.
/// The returned `(body_start, end_line)` pair is guaranteed to satisfy
/// `body_start <= end_line < total_lines`. Folds the [`is_r_chunk_language`]
/// guard so callers iterate all chunks and let-else past the non-R ones.
///
/// Note: this returns a range even for `eval=FALSE` chunks — callers that
/// need to suppress such chunks (e.g. [`mask_to_r`] for diagnostics) check
/// `chunk.eval_disabled` themselves.
pub(crate) fn r_chunk_body_range(chunk: &Chunk, total_lines: u32) -> Option<(u32, u32)> {
    if !is_r_chunk_language(&chunk.language) {
        return None;
    }
    let body_start = chunk.header_line.saturating_add(1);
    if body_start > chunk.end_line || body_start >= total_lines {
        return None;
    }
    Some((
        body_start,
        chunk.end_line.min(total_lines.saturating_sub(1)),
    ))
}

/// Produce R-parseable text from an R Markdown / Quarto document with
/// **identical line/column geometry** to the source.
///
/// ## Contract
///
/// * The input is split on `\n` (NOT `.lines()`, which strips `\r` and loses
///   information about trailing newlines). Any `\r` that precedes `\n` stays
///   attached to its segment.
/// * [`detect_chunks`] with [`ChunkKind::Rmd`] locates R chunk bodies. A
///   chunk is R when `is_r_chunk_language` returns `true` for its language
///   tag. Body lines are `(chunk.header_line + 1)..=chunk.end_line`, clamped
///   by `r_chunk_body_range` (shared with `semantic_tokens_for_rmd_document`):
///   - skip when `body_start > end_line` (empty chunk) or
///     `body_start >= total_lines` (header at EOF);
///   - clamp `end_line` to `total_lines - 1`.
/// * Each line is emitted **verbatim** (byte-identical, including any trailing
///   `\r`) when it falls inside an R chunk body; otherwise it is replaced by
///   an **empty string `""`**.
/// * Exception inside R bodies: a line matching the knitr chunk-reuse pattern
///   `^\s*<<[^>]+>>\s*$` (optionally with `\r`) is also blanked — it is
///   knitr meta-syntax, not R.
/// * Header fence lines, closing fence lines, prose, YAML front matter, and
///   non-R chunk bodies (Python, Bash, etc.) all become `""`.
/// * Segments are rejoined with `\n`. The result has exactly the same number
///   of `\n`-separated segments as the input (trailing-newline presence is
///   preserved automatically).
/// * A leading BOM (U+FEFF) lives on line 0, which is never an R body line,
///   so it is blanked — that is intentional and harmless for downstream R
///   parsing.
///
/// Net effect: identical line count; within kept R-body lines, identical
/// byte/column geometry. Downstream tools that parse the masked text obtain
/// positions they can use directly as document coordinates.
pub fn mask_to_r(text: &str) -> String {
    let segments: Vec<&str> = text.split('\n').collect();
    let total_lines = segments.len();

    // Build a boolean mask: true = keep this line verbatim.
    let mut keep = vec![false; total_lines];

    let reuse_re = chunk_reuse_re();
    let chunks = detect_chunks(text, ChunkKind::Rmd);

    for chunk in &chunks {
        let Some((body_start, end_line)) = r_chunk_body_range(chunk, total_lines as u32) else {
            continue;
        };
        // eval=FALSE chunks are display-only; their body may contain
        // intentionally malformed R. Blank them in the masked view so
        // diagnostics are suppressed, but leave r_chunk_body_range returning
        // the range so position-gated editor features (completion, semantic
        // tokens, indentation) still see the body.
        if chunk.eval_disabled {
            continue;
        }
        for idx in body_start as usize..=end_line as usize {
            // Blank knitr chunk-reuse references; keep everything else.
            if !reuse_re.is_match(segments[idx]) {
                keep[idx] = true;
            }
        }
    }

    let masked: Vec<&str> = segments
        .iter()
        .enumerate()
        .map(|(i, &seg)| if keep[i] { seg } else { "" })
        .collect();

    masked.join("\n")
}

/// Returns `true` iff the document's leading YAML frontmatter declares a
/// top-level `params:` key.
///
/// knitr/Quarto inject a `params` object into the document's R environment
/// whenever the YAML frontmatter declares a top-level `params:` mapping. The
/// masked analysis blanks the frontmatter, so the undefined-variable
/// diagnostic would otherwise flag every `params$...` use in a parameterized
/// report. Callers use this sniff (on the RAW document text) to recognize
/// `params` as a defined global for such documents. See
/// `diagnostics_from_snapshot` in `handlers.rs` for the injection point.
///
/// ## Frontmatter contract (single-key sniff, no YAML parser)
///
/// * The frontmatter is a leading YAML block: the first non-empty line — after
///   an optional leading BOM (U+FEFF) — must be exactly `---` (trimmed). If it
///   is not, the document has no frontmatter and this returns `false`.
/// * The block ends at the next line that is exactly `---` or `...` (trimmed).
///   A `params:` declaration must appear *inside* this block; a `params:` line
///   in prose after the frontmatter does not count.
/// * Within the block, a TOP-LEVEL `params:` key is a line whose first
///   character is `p` (column 0, no leading indentation) matching
///   `^params:\s*(#.*)?$` (key with no inline value, optional trailing
///   comment) or `^params:\s*\S` (key with an inline value). Nested/indented
///   `params:` keys, `# params:` comment lines, and `myparams:` do NOT match.
/// * This is intentionally a single-key sniff consistent with how the knit
///   pipeline (`editors/vscode/src/knit/yaml-frontmatter.ts`) sniffs
///   frontmatter delimiters — it pulls in no YAML crate.
pub fn frontmatter_declares_params(text: &str) -> bool {
    // Strip an optional leading BOM so the first delimiter is recognized.
    let text = crate::utf16::strip_leading_bom_for_scan(text);

    let mut lines = text.split('\n');

    // Find the opening delimiter: the first non-empty line must be `---`.
    let opened = loop {
        match lines.next() {
            Some(line) => {
                let trimmed = line.trim_end_matches('\r').trim();
                if trimmed.is_empty() {
                    continue;
                }
                break trimmed == "---";
            }
            None => break false,
        }
    };
    if !opened {
        return false;
    }

    // Scan the block until the closing delimiter (`---` or `...`).
    for line in lines {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            return false;
        }
        // A top-level `params:` key sits at column 0 (no indentation): a line
        // beginning with the literal `params:` prefix, whether bare (optionally
        // a trailing `# comment`) or with an inline value (`params: foo`). A
        // leading space (`  params:`), a comment (`# params:`), or a different
        // key (`myparams:`) lacks that prefix and so does not match.
        if line.starts_with("params:") {
            return true;
        }
    }
    false
}

/// Returns `true` iff `line` (0-based) falls within the body of an R chunk
/// in the given (raw) R Markdown / Quarto text.
///
/// "Body" means the lines strictly after the header fence and before (or at)
/// `end_line` — same bounds as [`mask_to_r`]. Header lines, closing fence
/// lines, prose, YAML front matter, and non-R chunk bodies all return `false`.
/// A knitr chunk-reuse line (`<<label>>`) inside an R body also returns
/// `false` — [`mask_to_r`] blanks it, so reporting it as R would activate
/// positional features (completion, signature help) on a line the analysis
/// view treats as blank. A `line` beyond the end of the document returns
/// `false`.
pub fn position_in_r_chunk_body(text: &str, line: u32) -> bool {
    let total_lines = text.split('\n').count();
    let chunks = detect_chunks(text, ChunkKind::Rmd);

    for chunk in &chunks {
        let Some((body_start, end_line)) = r_chunk_body_range(chunk, total_lines as u32) else {
            continue;
        };
        if line >= body_start && line <= end_line {
            // Same reuse-line test as `mask_to_r`: split on '\n' keeps any
            // trailing '\r', which the regex tolerates.
            return !text
                .split('\n')
                .nth(line as usize)
                .is_some_and(|l| chunk_reuse_re().is_match(l));
        }
    }
    false
}

/// A prose Markdown heading detected in an R Markdown / Quarto document.
///
/// Surfaced in the document outline (see `SymbolExtractor::extract_markdown_headings`)
/// so the rendered document's heading structure — the same structure the Knit
/// Preview shows — appears in VS Code's Outline view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownHeading {
    /// 0-based line index of the heading.
    pub line: u32,
    /// ATX heading level: 1 (`#`) through 6 (`######`).
    pub level: u32,
    /// Heading text, with the leading `#` run, any closing `#` sequence, and
    /// surrounding whitespace removed. Never empty (empty-title headings are
    /// dropped).
    pub title: String,
}

/// Detect prose (ATX) Markdown headings in raw R Markdown / Quarto text.
///
/// Only headings in *prose* are returned: lines inside leading YAML front
/// matter and inside fenced code blocks (both ` ```{r} ` chunks and generic
/// ` ``` ` / `~~~` blocks) are skipped, so a `#` comment inside code never
/// becomes a phantom heading. This intentionally complements [`mask_to_r`],
/// which keeps R chunk bodies and blanks prose; here we keep prose headings and
/// skip code.
///
/// Heading recognition follows CommonMark ATX rules: 1–6 leading `#` (after up
/// to 3 spaces of indentation) must be followed by a space/tab or end of line
/// (so `#hashtag` is not a heading), an optional trailing `#` closing sequence
/// is stripped only when preceded by whitespace, and empty-title headings are
/// dropped. Setext (underline) headings are not recognized.
///
/// Fence tracking mirrors `detect_rmd_chunks`'s close rules — a closing fence
/// must use the same character as its opener, be at least as long, and carry no
/// info string — so the two stay aligned on tilde/backtick and unclosed-to-EOF
/// behavior. An unclosed fence runs to end of document.
pub fn detect_markdown_headings(text: &str) -> Vec<MarkdownHeading> {
    // BOM-tolerant, column-0 scan: matching anchors at column 0 (after ≤3
    // spaces). Reported positions are line numbers only, so the stripped
    // first line is safe here. #346.
    let lines = crate::utf16::lines_for_column0_scan(text);
    let n = lines.len();

    // Skip a leading YAML frontmatter block, mirroring the contract in
    // `frontmatter_declares_params`: the first non-empty line must be exactly
    // `---`, and the block ends at the next `---`/`...`. An unterminated block
    // is not treated as frontmatter (scan from the top), matching that sniff.
    let mut scan_start = 0usize;
    let mut first_non_empty = 0usize;
    while first_non_empty < n && lines[first_non_empty].trim().is_empty() {
        first_non_empty += 1;
    }
    if first_non_empty < n && lines[first_non_empty].trim() == "---" {
        for (j, line) in lines.iter().enumerate().skip(first_non_empty + 1) {
            let t = line.trim();
            if t == "---" || t == "..." {
                scan_start = j + 1;
                break;
            }
        }
    }

    let mut headings = Vec::new();
    let mut fence: Option<(char, usize)> = None; // (fence char, opening length)

    for (idx, &line) in lines.iter().enumerate().skip(scan_start) {
        if let Some((ch, len, has_info)) = fence_marker(line) {
            match fence {
                // A closing fence: same character, at least as long, no info string.
                Some((open_ch, open_len)) if ch == open_ch && len >= open_len && !has_info => {
                    fence = None;
                }
                Some(_) => {} // a fence-looking line inside a different fence stays content
                None => fence = Some((ch, len)),
            }
            continue; // fence lines are never headings
        }
        if fence.is_some() {
            continue;
        }
        if let Some((level, title)) = parse_atx_heading(line) {
            headings.push(MarkdownHeading {
                line: idx as u32,
                level,
                title,
            });
        }
    }

    headings
}

/// Classify a line as a Markdown code-fence marker: a run of ≥3 backticks or
/// tildes at column 0. Returns the fence character, the run length, and whether
/// a non-whitespace info string follows (a closing fence must have none).
///
/// Fences are recognized only at column 0, matching [`detect_rmd_chunks`]
/// (`fence_header_re`/`fence_close_re` both anchor at column 0). Keeping the two
/// detectors aligned means heading detection and chunk-range detection never
/// disagree about where a fenced block opens or closes — e.g. an indented ```
/// closes neither, so both let the fence run on rather than one treating the
/// following text as prose and the other as code.
fn fence_marker(line: &str) -> Option<(char, usize, bool)> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = line.chars().take_while(|&c| c == first).count();
    if run < 3 {
        return None;
    }
    // `first` is a single-byte ASCII char, so `run` bytes index the rest safely.
    let has_info = !line[run..].trim().is_empty();
    Some((first, run, has_info))
}

/// Parse a line as an ATX Markdown heading, returning `(level, title)` when it
/// is one with a non-empty title. Follows CommonMark: ≤3 spaces of indentation,
/// 1–6 `#`, a required space/tab (or end of line) after the `#` run, and an
/// optional trailing `#` closing sequence stripped only when whitespace-separated.
fn parse_atx_heading(line: &str) -> Option<(u32, String)> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None; // 4+ spaces is an indented code block, not a heading
    }
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let after = &trimmed[hashes..]; // '#' is single-byte ASCII
    // The `#` run must be followed by whitespace or end of line.
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    let content = after.trim_matches([' ', '\t']);
    let title = strip_closing_hashes(content).trim_matches([' ', '\t']);
    if title.is_empty() {
        return None;
    }
    Some((hashes as u32, title.to_string()))
}

/// Strip a trailing ATX closing sequence (`#`s) from heading content. The
/// closing sequence is removed only when it is whitespace-separated from the
/// content or constitutes the entire content; otherwise the `#`s are content
/// (e.g. `bar#`).
fn strip_closing_hashes(content: &str) -> &str {
    let without = content.trim_end_matches('#');
    if without.len() == content.len() {
        return content; // no trailing '#'
    }
    if without.is_empty() {
        return ""; // content was only '#'s
    }
    if without.ends_with([' ', '\t']) {
        return without.trim_end_matches([' ', '\t']);
    }
    content // trailing '#' not whitespace-separated → part of the content
}

/// Regex for the in-chunk `# raven: ignore-chunk` directive (F2 Step 4),
/// optionally with a `[code]` selector. Anchored to the start of a (possibly
/// indented) line; a trailing comment-only directive.
fn ignore_chunk_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*#\s*raven:\s*ignore-chunk(?:\[([^\]]*)\])?\s*\r?$")
            .expect("ignore-chunk regex")
    })
}

/// Parse a comma-separated `[code]` body (or `None`/empty) into a
/// [`crate::cross_file::types::LineSuppression`], normalizing each code to canonical kebab-case. Mirrors
/// the directive/lint-track parsers; empty → blanket.
fn chunk_codes_or_all(body: Option<&str>) -> crate::cross_file::types::LineSuppression {
    use crate::cross_file::types::LineSuppression;
    match body {
        None => LineSuppression::All,
        Some(b) => {
            let codes: Vec<String> = b
                .split(',')
                .map(crate::diagnostic_code::normalize)
                .filter(|c| !c.is_empty())
                .collect();
            if codes.is_empty() {
                LineSuppression::All
            } else {
                LineSuppression::Codes(codes)
            }
        }
    }
}

/// If `line` is an in-chunk `# raven: ignore-chunk` directive, return what it
/// suppresses; otherwise `None`.
fn parse_ignore_chunk_directive(line: &str) -> Option<crate::cross_file::types::LineSuppression> {
    let caps = ignore_chunk_re().captures(line)?;
    Some(chunk_codes_or_all(caps.get(1).map(|m| m.as_str())))
}

/// Parse the `raven.ignore` knitr chunk option out of a chunk header's option
/// string (group 3 of [`fence_header_re`]). Returns:
/// * `Some(All)` for `raven.ignore=TRUE`/`=T` or a bare `raven.ignore`;
/// * `Some(Codes(..))` for `raven.ignore="a,b"` / `='a'` (quoted code list);
/// * `None` when the option is absent, explicitly `FALSE`/`F`, OR an explicitly
///   empty/whitespace-only quoted code list (`raven.ignore=""`). An empty *value*
///   is an empty code list — it suppresses nothing, the inverse of a blanket
///   ignore. (This differs from an empty `[code]` *bracket selector* on
///   `# raven: ignore[]` / `ignore-chunk[]`, which means blanket per
///   [`chunk_codes_or_all`] and the directive-track parser; brackets are a
///   "narrow this" qualifier whose absence-or-emptiness defaults to all, whereas
///   a quoted value is the list itself.)
///
/// Uses the same bracket/quote-aware comma split as [`has_eval_false`] so a
/// value like `fig.dim=c(5, 6)` doesn't confuse the scan.
fn chunk_ignore_option(header_rest: &str) -> Option<crate::cross_file::types::LineSuppression> {
    use crate::cross_file::types::LineSuppression;
    let parts = split_header_options(header_rest);
    for raw in &parts {
        let part = raw.trim();
        let Some(val) = part.strip_prefix("raven.ignore") else {
            continue;
        };
        let val = val.trim_start();
        // Bare `raven.ignore` (no `=`) means blanket-on.
        let Some(val) = val.strip_prefix('=') else {
            if val.is_empty() {
                return Some(LineSuppression::All);
            }
            // `raven.ignoreX` — not our option.
            continue;
        };
        let val = val.trim();
        return match val {
            "TRUE" | "T" => Some(LineSuppression::All),
            "FALSE" | "F" => None,
            _ => {
                // Quoted code list: strip surrounding quotes, split on commas.
                let unquoted = val
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| val.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(val);
                // An explicit empty/whitespace-only code list (`raven.ignore=""`)
                // suppresses nothing — do not collapse it to a blanket ignore.
                if unquoted.trim().is_empty() {
                    return None;
                }
                Some(chunk_codes_or_all(Some(unquoted)))
            }
        };
    }
    None
}

/// Bracket/quote-aware comma split of a chunk-header option string. Shared by
/// [`chunk_ignore_option`]; identical logic to the split inside
/// [`has_eval_false`]/[`parse_header_label`].
fn split_header_options(header_rest: &str) -> Vec<String> {
    let trimmed = header_rest.trim();
    let mut parts: Vec<String> = Vec::new();
    if trimmed.is_empty() {
        return parts;
    }
    let mut current = String::new();
    let mut in_quote: Option<char> = None;
    let mut depth = 0i32;
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = in_quote {
            current.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                current.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if ch == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        match ch {
            '"' | '\'' => {
                in_quote = Some(ch);
                current.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' => {
                if depth > 0 {
                    depth -= 1;
                }
                current.push(ch);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
        i += 1;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// F2 Step 4: append chunk-level suppression ranges (and directives) derived
/// from an R Markdown / Quarto document's **raw** text to `meta`.
///
/// Two forms, both mapping the whole R chunk *body* onto the existing
/// [`SuppressionRange`](crate::cross_file::types::SuppressionRange) machinery so
/// the per-code analyzer/lint enforcement applies unchanged:
/// 1. a knitr chunk **option** in the header — `{r, raven.ignore=TRUE}`
///    (blanket) or `{r, raven.ignore="undefined-variable"}` (per-code); and
/// 2. an in-chunk **directive** `# raven: ignore-chunk` (optionally `[code]`)
///    anywhere in the chunk body.
///
/// Chunk suppressions are always the [`crate::cross_file::types::SuppressionFlavor::Ignore`] flavor
/// (silent). Must be called on the RAW document text: the chunk header (which
/// carries the option) is blanked in the masked analysis text. The directive
/// hint anchor is the chunk header line. `# raven: ignore-start/end` blocks
/// inside a chunk are handled by the normal directive parser (chunk bodies
/// survive masking), so they need no special handling here.
pub fn append_chunk_suppressions(
    meta: &mut crate::cross_file::types::CrossFileMetadata,
    raw_text: &str,
) {
    use crate::cross_file::types::{
        LineSuppression, SuppressionDirective, SuppressionFlavor, SuppressionRange,
    };
    let lines: Vec<&str> = raw_text.split('\n').collect();
    let total_lines = lines.len() as u32;
    let chunks = detect_chunks(raw_text, ChunkKind::Rmd);
    let header_re = fence_header_re();

    for chunk in &chunks {
        let Some((body_start, end_line)) = r_chunk_body_range(chunk, total_lines) else {
            continue;
        };
        // eval=FALSE bodies are already blanked in mask_to_r — no diagnostics
        // are produced for them, so no suppressions are needed.
        if chunk.eval_disabled {
            continue;
        }

        // Form 1: header option (`raven.ignore=...`). Strip a leading BOM so a
        // chunk header on line 0 of a BOM-prefixed document still matches the
        // `^`-anchored fence regex (mirrors `detect_chunks`, which is
        // BOM-tolerant via `lines_for_column0_scan`).
        let header_opts = lines
            .get(chunk.header_line as usize)
            .map(|h| h.strip_prefix('\u{FEFF}').unwrap_or(h))
            .and_then(|h| header_re.captures(h))
            .and_then(|c| c.get(3).map(|m| m.as_str().to_string()))
            .unwrap_or_default();
        let mut what: Option<LineSuppression> = chunk_ignore_option(&header_opts);

        // Form 2: in-chunk `# raven: ignore-chunk` (merged with any header option).
        for idx in body_start..=end_line {
            if let Some(w) = lines
                .get(idx as usize)
                .and_then(|l| parse_ignore_chunk_directive(l))
            {
                what = Some(match what {
                    Some(mut existing) => {
                        existing.merge(w);
                        existing
                    }
                    None => w,
                });
            }
        }

        if let Some(what) = what {
            meta.ignored_ranges.push(SuppressionRange {
                start: body_start,
                end: end_line,
                what: what.clone(),
            });
            meta.suppression_directives.push(SuppressionDirective {
                directive_line: chunk.header_line,
                target_start: body_start,
                target_end: end_line,
                what,
                flavor: SuppressionFlavor::Ignore,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(text: &str, kind: ChunkKind) -> Vec<Chunk> {
        detect_chunks(text, kind)
    }

    #[test]
    fn classifies_rmd_qmd_extensions() {
        assert_eq!(classify_chunk_document("/tmp/foo.Rmd"), ChunkKind::Rmd);
        assert_eq!(
            classify_chunk_document("/tmp/foo.Rmarkdown"),
            ChunkKind::Rmd
        );
        assert_eq!(
            classify_chunk_document("/tmp/foo.RMARKDOWN"),
            ChunkKind::Rmd
        );
        assert_eq!(classify_chunk_document("/tmp/foo.qmd"), ChunkKind::Rmd);
        assert_eq!(classify_chunk_document("/tmp/foo.QMD"), ChunkKind::Rmd);
        assert_eq!(classify_chunk_document("/tmp/foo.R"), ChunkKind::R);
        assert_eq!(classify_chunk_document("/tmp/foo.r"), ChunkKind::R);
        assert_eq!(classify_chunk_document("/tmp/foo.txt"), ChunkKind::R);
    }

    #[test]
    fn classifies_untitled_buffers_by_language_id() {
        // Untitled buffers have no extension, so `languageId` is the only
        // signal we have for distinguishing Rmd/Quarto from plain R.
        assert_eq!(
            classify_chunk_document_for(Some("rmd"), "untitled:Untitled-1"),
            ChunkKind::Rmd
        );
        assert_eq!(
            classify_chunk_document_for(Some("quarto"), "untitled:Untitled-1"),
            ChunkKind::Rmd
        );
        assert_eq!(
            classify_chunk_document_for(Some("RMD"), "untitled:Untitled-1"),
            ChunkKind::Rmd
        );
        assert_eq!(
            classify_chunk_document_for(Some("r"), "untitled:Untitled-1"),
            ChunkKind::R
        );
        assert_eq!(
            classify_chunk_document_for(None, "/tmp/foo.Rmd"),
            ChunkKind::Rmd
        );
        // languageId='r' on a .Rmd URI: trust the URI (matches the TS detector).
        assert_eq!(
            classify_chunk_document_for(Some("r"), "/tmp/foo.Rmd"),
            ChunkKind::Rmd
        );
    }

    #[test]
    fn detects_a_single_r_chunk() {
        let src = "Some prose.\n\n```{r}\nx <- 1\nprint(x)\n```\n\nMore prose.";
        let chunks = detect(src, ChunkKind::Rmd);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].header_line, 2);
        assert_eq!(chunks[0].closing_fence_line, Some(5));
        assert_eq!(chunks[0].end_line, 4);
        assert_eq!(chunks[0].language, "r");
    }

    // Issue #346: the fence-header and cell-marker regexes anchor at column 0
    // (`^`). A raw leading U+FEFF on the first line of in-memory text (open
    // documents keep the BOM verbatim) would otherwise defeat the `^` anchor and
    // hide a first-line chunk/cell.
    #[test]
    fn detects_first_line_fence_header_after_bom() {
        let src = "\u{FEFF}```{r}\nx <- 1\n```\n";
        let chunks = detect(src, ChunkKind::Rmd);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].header_line, 0);
        assert_eq!(chunks[0].language, "r");
    }

    #[test]
    fn detects_first_line_cell_marker_after_bom() {
        let src = "\u{FEFF}# %% setup\nx <- 1\n";
        let chunks = detect(src, ChunkKind::R);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].header_line, 0);
        assert_eq!(chunks[0].label.as_deref(), Some("setup"));
    }

    #[test]
    fn parses_label_from_header() {
        let src = "```{r setup, eval=FALSE, fig.width=4}\nx <- 1\n```";
        let chunks = detect(src, ChunkKind::Rmd);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label.as_deref(), Some("setup"));
    }

    #[test]
    fn skips_options_when_picking_label() {
        // First option is key=value, so the label is None (no bare token).
        let src = "```{r, eval=FALSE}\n1\n```";
        let chunks = detect(src, ChunkKind::Rmd);
        assert_eq!(chunks[0].label, None);
    }

    #[test]
    fn handles_unclosed_chunk_at_eof() {
        let src = "```{r}\nx <- 1\nprint(x)";
        let chunks = detect(src, ChunkKind::Rmd);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].header_line, 0);
        assert_eq!(chunks[0].closing_fence_line, None);
        assert_eq!(chunks[0].end_line, 2);
    }

    #[test]
    fn tracks_non_r_language_tag() {
        let src = "```{python}\nprint('hi')\n```\n\n```{julia}\n1+1\n```";
        let chunks = detect(src, ChunkKind::Rmd);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].language, "python");
        assert_eq!(chunks[1].language, "julia");
    }

    #[test]
    fn tilde_fences_work() {
        let src = "~~~{r}\nx <- 1\n~~~";
        let chunks = detect(src, ChunkKind::Rmd);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].closing_fence_line, Some(2));
    }

    #[test]
    fn fence_close_requires_same_char_and_min_length() {
        // 4-backtick opener; 3-backtick close should not match.
        let src = "````{r}\nx <- 1\n```\n````";
        let chunks = detect(src, ChunkKind::Rmd);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].closing_fence_line, Some(3));
    }

    #[test]
    fn detects_r_cells_basic() {
        let src = "# %% Setup\nlibrary(dplyr)\n\n# %% Analysis\nx <- 1";
        let chunks = detect(src, ChunkKind::R);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].header_line, 0);
        assert_eq!(chunks[0].end_line, 2);
        assert_eq!(chunks[0].label.as_deref(), Some("Setup"));
        assert_eq!(chunks[1].header_line, 3);
        assert_eq!(chunks[1].end_line, 4);
        assert_eq!(chunks[1].label.as_deref(), Some("Analysis"));
    }

    #[test]
    fn empty_cell_marker_has_no_label() {
        let src = "# %%\nx <- 1";
        let chunks = detect(src, ChunkKind::R);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, None);
    }

    #[test]
    fn roxygen_line_does_not_end_cell() {
        // A `#' @param x A value -----` line inside a cell must NOT be
        // mistaken for a section divider — the divider regex must require a
        // non-quote character right after the leading hashes.
        let src = "# %% Setup\n#' @param x A value -----\nlibrary(x)\n# %% Next\ny <- 2";
        let chunks = detect(src, ChunkKind::R);
        assert_eq!(chunks.len(), 2);
        // First cell should extend through line 2 (`library(x)`), not be
        // truncated at the roxygen line.
        assert_eq!(chunks[0].end_line, 2);
    }

    #[test]
    fn section_divider_ends_cell() {
        let src = "# %% Setup\nlibrary(x)\n# Header ----\nx <- 1\n# %% Next\ny <- 2";
        let chunks = detect(src, ChunkKind::R);
        assert_eq!(chunks.len(), 2);
        // The divider line itself is the last line of the first cell.
        assert_eq!(chunks[0].end_line, 2);
    }

    #[test]
    fn cell_marker_with_trailing_decoration_keeps_label() {
        let src = "# %% Setup ----\nx <- 1";
        let chunks = detect(src, ChunkKind::R);
        assert_eq!(chunks[0].label.as_deref(), Some("Setup"));
    }

    #[test]
    fn parenthesised_option_values_survive() {
        let src = "```{r, fig.dim=c(5, 6), out.width=\"80%\"}\n1\n```";
        let chunks = detect(src, ChunkKind::Rmd);
        // We don't store options, but the header must still parse cleanly
        // and produce one chunk without confusing the comma split.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].language, "r");
    }

    // =========================================================================
    // mask_to_r tests
    // =========================================================================

    /// Helper: count of '\n'-separated segments (mirrors split('\n').count()).
    fn seg_count(s: &str) -> usize {
        s.split('\n').count()
    }

    #[test]
    fn mask_preserves_line_count() {
        let cases: &[&str] = &[
            // no trailing newline
            "---\ntitle: T\n---\n\n```{r}\nx <- 1\n```\n\nMore prose.",
            // trailing newline
            "---\ntitle: T\n---\n\n```{r}\nx <- 1\n```\n\nMore prose.\n",
            // empty string
            "",
            // single line, no newline
            "just prose",
            // single newline
            "\n",
            // empty chunk
            "```{r}\n```\n",
            // unclosed chunk at EOF
            "```{r}\nx <- 1",
        ];
        for &t in cases {
            let masked = mask_to_r(t);
            assert_eq!(
                seg_count(&masked),
                seg_count(t),
                "line count mismatch for input {t:?}"
            );
        }
    }

    #[test]
    fn mask_keeps_r_body_byte_identical() {
        // Body line has leading and trailing spaces — must survive verbatim.
        let src = "```{r}\n  x <- f(1)  \n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        // Line 0 = header  → ""
        // Line 1 = body    → "  x <- f(1)  " (verbatim)
        // Line 2 = close   → ""
        // Line 3 = trailing → ""
        assert_eq!(lines[1], "  x <- f(1)  ");
    }

    #[test]
    fn mask_blanks_prose_yaml_and_fences() {
        let src = "---\ntitle: Test\n---\n\n```{r}\nx <- 1\n```\n\nProse.";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        // Line 0 = "---"          → ""
        // Line 1 = "title: Test"  → ""
        // Line 2 = "---"          → ""
        // Line 3 = ""  (blank prose line) → ""
        // Line 4 = "```{r}"       → "" (header fence)
        // Line 5 = "x <- 1"       → "x <- 1" (R body)
        // Line 6 = "```"          → "" (closing fence)
        // Line 7 = ""  (blank)    → ""
        // Line 8 = "Prose."       → ""
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "");
        assert_eq!(lines[4], "");
        assert_eq!(lines[5], "x <- 1");
        assert_eq!(lines[6], "");
        assert_eq!(lines[7], "");
        assert_eq!(lines[8], "");
    }

    #[test]
    fn mask_blanks_non_r_chunks() {
        let src = "```{python}\nprint('hi')\n```\n\n```{r}\ny <- 2\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        // python body (line 1) → ""
        assert_eq!(lines[1], "");
        // R body (line 5) → "y <- 2"
        assert_eq!(lines[5], "y <- 2");
    }

    #[test]
    fn mask_crlf_preserves_cr_in_body() {
        // CRLF document: each raw line ends with '\r'.
        let src = "prose\r\n```{r}\r\nx <- 1\r\n```\r\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        // Line count unchanged.
        assert_eq!(seg_count(&masked), seg_count(src));
        // Line 0 = "prose\r"   → "" (prose)
        assert_eq!(lines[0], "");
        // Line 1 = "```{r}\r"  → "" (header)
        assert_eq!(lines[1], "");
        // Line 2 = "x <- 1\r"  → "x <- 1\r" (body, \r preserved)
        assert_eq!(lines[2], "x <- 1\r");
        // Line 3 = "```\r"     → "" (closing fence)
        assert_eq!(lines[3], "");
        // Line 4 = "" (trailing after final \n)
        assert_eq!(lines[4], "");
        // Blanked lines are "" not "\r".
        for (i, &l) in lines.iter().enumerate() {
            if i != 2 {
                assert!(!l.contains('\r'), "blanked line {i} must not contain \\r");
            }
        }
    }

    #[test]
    fn mask_bom_first_line() {
        // BOM on line 0; chunk starts on line 1.
        let src = "\u{FEFF}\n```{r}\nx <- 1\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        // No panic; geometry intact.
        assert_eq!(seg_count(&masked), seg_count(src));
        // Line 0 contains BOM → blanked.
        assert_eq!(lines[0], "");
        // Line 2 = R body → "x <- 1"
        assert_eq!(lines[2], "x <- 1");
    }

    #[test]
    fn mask_tilde_fences() {
        let src = "~~~{r}\nx <- 1\n~~~\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(lines[0], ""); // header
        assert_eq!(lines[1], "x <- 1"); // body
        assert_eq!(lines[2], ""); // closing
    }

    #[test]
    fn mask_nested_fences() {
        // 4-backtick opener; inner ``` is body content, not a closer.
        let src = "````{r}\nx <- 1\n```\ny <- 2\n````\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        // detect_chunks handles the nesting: closer is at line 4.
        assert_eq!(lines[0], ""); // header (````{r})
        assert_eq!(lines[1], "x <- 1"); // body
        assert_eq!(lines[2], "```"); // inner ``` preserved as body
        assert_eq!(lines[3], "y <- 2"); // body
        assert_eq!(lines[4], ""); // closing (````)
    }

    #[test]
    fn mask_unclosed_chunk_at_eof() {
        // No closing fence: body runs to EOF.
        let src = "```{r}\nx <- 1\nprint(x)";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(seg_count(&masked), seg_count(src));
        assert_eq!(lines[0], ""); // header
        assert_eq!(lines[1], "x <- 1"); // body
        assert_eq!(lines[2], "print(x)"); // body at EOF
    }

    #[test]
    fn mask_empty_chunk() {
        // Header immediately followed by closing fence — no body lines.
        let src = "```{r}\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(seg_count(&masked), seg_count(src));
        assert_eq!(lines[0], ""); // header
        assert_eq!(lines[1], ""); // closing fence
    }

    #[test]
    fn mask_multiple_chunks() {
        // Two R chunks separated by prose; both bodies at original indices.
        let src = "```{r}\na <- 1\n```\n\nprose\n\n```{r}\nb <- 2\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(seg_count(&masked), seg_count(src));
        assert_eq!(lines[0], ""); // first header
        assert_eq!(lines[1], "a <- 1"); // first body
        assert_eq!(lines[2], ""); // first close
        assert_eq!(lines[3], ""); // blank prose
        assert_eq!(lines[4], ""); // prose
        assert_eq!(lines[5], ""); // blank prose
        assert_eq!(lines[6], ""); // second header
        assert_eq!(lines[7], "b <- 2"); // second body
        assert_eq!(lines[8], ""); // second close
    }

    #[test]
    fn mask_blanks_chunk_reuse_reference() {
        // <<setup>> inside an R body must be blanked.
        let src = "```{r}\nx <- 1\n<<setup>>\ny <- 2\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(lines[1], "x <- 1"); // kept
        assert_eq!(lines[2], ""); // <<setup>> → blanked
        assert_eq!(lines[3], "y <- 2"); // kept
    }

    // =========================================================================
    // eval=FALSE chunk blanking tests
    // =========================================================================

    #[test]
    fn mask_blanks_eval_false_chunk() {
        // eval=FALSE chunk body contains malformed R; mask must blank it.
        let src = "```{r, eval = FALSE}\ndf |> unnest(x))\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(seg_count(&masked), seg_count(src));
        assert_eq!(lines[0], ""); // header
        assert_eq!(lines[1], ""); // body → blanked (eval=FALSE)
        assert_eq!(lines[2], ""); // closing fence
    }

    #[test]
    fn mask_blanks_eval_f_chunk() {
        // eval=F (shorthand) should also be blanked.
        let src = "```{r, eval=F}\nmy_func <- function(...) {\n}\n}\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(seg_count(&masked), seg_count(src));
        // All body lines blanked.
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "");
    }

    #[test]
    fn mask_keeps_eval_true_chunk() {
        // eval=TRUE (explicit) must still preserve the body.
        let src = "```{r, eval = TRUE}\nx <- 1\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(lines[1], "x <- 1");
    }

    #[test]
    fn mask_keeps_chunk_without_eval_option() {
        // No eval option at all → body preserved (default eval=TRUE).
        let src = "```{r setup}\nx <- 1\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(lines[1], "x <- 1");
    }

    #[test]
    fn mask_blanks_eval_false_with_other_options() {
        // eval=FALSE mixed with other options; only eval matters for blanking.
        let src = "```{r, fig.width=10, eval = FALSE, echo=TRUE}\nread_excel(..., range = cell_cols(c(\"A\", \"Z\"))\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        // Body line has unclosed paren — would be a syntax error if not blanked.
        assert_eq!(lines[1], "");
    }

    #[test]
    fn has_eval_false_detects_variants() {
        assert!(has_eval_false(", eval = FALSE"));
        assert!(has_eval_false(", eval=FALSE"));
        assert!(has_eval_false(", eval = F"));
        assert!(has_eval_false(", eval=F"));
        assert!(has_eval_false(" setup, eval = FALSE, echo=TRUE"));
        assert!(has_eval_false(", fig.dim=c(5, 6), eval=FALSE"));
    }

    #[test]
    fn has_eval_false_rejects_non_false() {
        assert!(!has_eval_false(", eval = TRUE"));
        assert!(!has_eval_false(", eval=TRUE"));
        assert!(!has_eval_false(", eval = T"));
        assert!(!has_eval_false(""));
        assert!(!has_eval_false(", echo = FALSE")); // not eval
        assert!(!has_eval_false(", eval = is_ci()")); // dynamic
    }

    // =========================================================================
    // position_in_r_chunk_body tests
    // =========================================================================

    #[test]
    fn position_in_r_chunk_body_true_for_body_line() {
        let src = "prose\n```{r}\nx <- 1\n```\n";
        // Line 2 is the R body.
        assert!(position_in_r_chunk_body(src, 2));
    }

    #[test]
    fn position_in_r_chunk_body_false_for_header_line() {
        let src = "prose\n```{r}\nx <- 1\n```\n";
        // Line 1 is the header fence.
        assert!(!position_in_r_chunk_body(src, 1));
    }

    #[test]
    fn position_in_r_chunk_body_false_for_closing_fence_line() {
        let src = "prose\n```{r}\nx <- 1\n```\n";
        // Line 3 is the closing fence.
        assert!(!position_in_r_chunk_body(src, 3));
    }

    #[test]
    fn position_in_r_chunk_body_false_for_prose_line() {
        let src = "prose\n```{r}\nx <- 1\n```\n";
        // Line 0 is prose.
        assert!(!position_in_r_chunk_body(src, 0));
    }

    #[test]
    fn position_in_r_chunk_body_false_for_python_body_line() {
        let src = "```{python}\nprint('hi')\n```\n";
        // Line 1 is a Python body — not R.
        assert!(!position_in_r_chunk_body(src, 1));
    }

    #[test]
    fn position_in_r_chunk_body_false_beyond_eof() {
        let src = "```{r}\nx <- 1\n```\n";
        // Line 99 is way past the end.
        assert!(!position_in_r_chunk_body(src, 99));
    }

    #[test]
    fn position_in_r_chunk_body_true_for_eval_false_chunk() {
        // eval=FALSE chunks are display-only but position-gated editor features
        // (completion, semantic tokens, indentation) must still see the body.
        let src = "```{r, eval=FALSE}\nx <- 1\n```\n";
        assert!(position_in_r_chunk_body(src, 1));
    }

    #[test]
    fn mask_bom_on_fence_header_line() {
        // BOM lives on line 0, which is the header fence itself. The header
        // is never an R body line, so line 0 must be blanked even though the
        // BOM-stripped text would match the fence pattern.
        let src = "\u{FEFF}```{r}\nx <- 1\n```\n";
        let masked = mask_to_r(src);
        let lines: Vec<&str> = masked.split('\n').collect();
        assert_eq!(seg_count(&masked), seg_count(src));
        assert_eq!(lines[0], ""); // header with BOM → blanked
        assert_eq!(lines[1], "x <- 1"); // R body → kept verbatim
        assert_eq!(lines[2], ""); // closing fence → blanked
    }

    #[test]
    fn position_in_r_chunk_body_unclosed_chunk() {
        // Unclosed chunk: body runs to EOF; line 1 is inside the body.
        let src = "```{r}\nx <- 1";
        assert!(position_in_r_chunk_body(src, 1));
    }

    #[test]
    fn position_in_r_chunk_body_false_for_chunk_reuse_line() {
        // A knitr `<<label>>` reuse line sits inside the body range but is
        // blanked by mask_to_r — the guard must agree with the masked view.
        let src = "```{r}\nx <- 1\n<<setup>>\n```\n";
        assert!(position_in_r_chunk_body(src, 1)); // real R body line
        assert!(!position_in_r_chunk_body(src, 2)); // reuse line → non-R
        // CRLF variant: split('\n') keeps the '\r'; the regex tolerates it.
        let crlf = "```{r}\r\n<<setup>>\r\n```\r\n";
        assert!(!position_in_r_chunk_body(crlf, 1));
    }

    // =========================================================================
    // frontmatter_declares_params tests
    // =========================================================================

    #[test]
    fn params_declared_with_nested_keys() {
        let src = "---\ntitle: My report\nparams:\n  year: 2024\n  region: \"north\"\n---\n\n```{r}\nprint(params$year)\n```\n";
        assert!(frontmatter_declares_params(src));
    }

    #[test]
    fn params_declared_with_inline_value() {
        // `params:` followed by an inline value (unusual but valid YAML).
        let src = "---\nparams: ~\n---\n";
        assert!(frontmatter_declares_params(src));
    }

    #[test]
    fn params_declared_with_inline_comment() {
        let src = "---\ntitle: T\nparams:   # report parameters\n  year: 2024\n---\n";
        assert!(frontmatter_declares_params(src));
    }

    #[test]
    fn params_declared_with_bom_before_delimiter() {
        let src = "\u{FEFF}---\nparams:\n  year: 2024\n---\n```{r}\nparams$year\n```\n";
        assert!(frontmatter_declares_params(src));
    }

    #[test]
    fn params_declared_after_leading_blank_lines() {
        // Blank lines before the opening `---` are tolerated.
        let src = "\n\n---\nparams:\n  year: 2024\n---\n";
        assert!(frontmatter_declares_params(src));
    }

    #[test]
    fn params_declared_handles_crlf() {
        let src = "---\r\nparams:\r\n  year: 2024\r\n---\r\n";
        assert!(frontmatter_declares_params(src));
    }

    #[test]
    fn params_closing_delimiter_dots() {
        // YAML allows `...` as a document end marker; `params:` before it counts.
        let src = "---\nparams:\n  year: 2024\n...\n```{r}\nparams$year\n```\n";
        assert!(frontmatter_declares_params(src));
    }

    #[test]
    fn no_params_when_no_frontmatter() {
        let src = "```{r}\nparams$year\n```\n";
        assert!(!frontmatter_declares_params(src));
    }

    #[test]
    fn no_params_when_frontmatter_lacks_params() {
        let src = "---\ntitle: My report\nauthor: A. Author\n---\n```{r}\nx <- 1\n```\n";
        assert!(!frontmatter_declares_params(src));
    }

    #[test]
    fn no_params_when_params_is_nested() {
        // `params:` indented under another key is NOT a top-level declaration.
        let src = "---\nrmarkdown:\n  params:\n    year: 2024\n---\n";
        assert!(!frontmatter_declares_params(src));
    }

    #[test]
    fn no_params_when_params_only_in_prose() {
        // `params:` appears after the frontmatter closes — it's prose, not YAML.
        let src = "---\ntitle: T\n---\n\nThe params: are documented below.\nparams: not yaml\n";
        assert!(!frontmatter_declares_params(src));
    }

    #[test]
    fn no_params_when_params_is_comment_in_frontmatter() {
        // A `# params:` comment line inside the frontmatter does NOT declare
        // the key (it starts with `#`, not `params:` at column 0). Documented
        // behavior: comment lines are ignored.
        let src = "---\ntitle: T\n# params: would go here\n---\n";
        assert!(!frontmatter_declares_params(src));
    }

    #[test]
    fn no_params_when_key_is_a_different_name() {
        // `myparams:` (prefix) and `params_extra:` (suffix) must not match the
        // literal `params:` key at column 0.
        let src = "---\nmyparams:\n  year: 2024\n---\n";
        assert!(!frontmatter_declares_params(src));
        let src2 = "---\nparams_extra: 1\n---\n";
        assert!(!frontmatter_declares_params(src2));
    }

    #[test]
    fn no_params_for_empty_document() {
        assert!(!frontmatter_declares_params(""));
    }

    #[test]
    fn no_params_when_first_nonempty_line_is_not_delimiter() {
        // A document that opens with prose has no frontmatter block at all.
        let src = "# Heading\n---\nparams:\n  year: 2024\n---\n";
        assert!(!frontmatter_declares_params(src));
    }

    // =========================================================================
    // F2 Step 4: chunk-level suppression
    // =========================================================================

    use crate::cross_file::types::{LineSuppression, SuppressionFlavor};

    #[test]
    fn chunk_option_blanket_suppresses_whole_body() {
        let src = "```{r, raven.ignore=TRUE}\nx <- undefined\ny <- also_undefined\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert_eq!(meta.ignored_ranges.len(), 1);
        let r = &meta.ignored_ranges[0];
        assert_eq!((r.start, r.end), (1, 2));
        assert_eq!(r.what, LineSuppression::All);
        // Enumerated as an Ignore-flavored directive anchored at the header.
        assert_eq!(meta.suppression_directives.len(), 1);
        assert_eq!(meta.suppression_directives[0].directive_line, 0);
        assert_eq!(
            meta.suppression_directives[0].flavor,
            SuppressionFlavor::Ignore
        );
    }

    #[test]
    fn chunk_option_per_code_suppresses_only_listed_codes() {
        let src = "```{r, raven.ignore=\"undefined-variable\"}\nx <- undefined\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert_eq!(meta.ignored_ranges.len(), 1);
        let r = &meta.ignored_ranges[0];
        assert!(r.what.covers(Some("undefined-variable")));
        assert!(!r.what.covers(Some("line-length")));
    }

    #[test]
    fn chunk_option_false_does_not_suppress() {
        let src = "```{r, raven.ignore=FALSE}\nx <- undefined\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert!(meta.ignored_ranges.is_empty());
    }

    #[test]
    fn chunk_option_empty_quoted_list_suppresses_nothing() {
        // FIX 3: `raven.ignore=""` is an explicitly EMPTY code list, not a
        // blanket ignore. It must suppress nothing (parses to None).
        assert_eq!(chunk_ignore_option("raven.ignore=\"\""), None);
        assert_eq!(chunk_ignore_option("raven.ignore=''"), None);
        // Whitespace-only is also an empty list.
        assert_eq!(chunk_ignore_option("raven.ignore=\"  \""), None);
        // And end-to-end: an empty-list chunk produces no suppression range.
        let src = "```{r, raven.ignore=\"\"}\nx <- undefined\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert!(
            meta.ignored_ranges.is_empty(),
            "raven.ignore=\"\" must not create a suppression range, got {:?}",
            meta.ignored_ranges
        );
        // Contrast: a non-empty list still suppresses the listed code.
        assert!(matches!(
            chunk_ignore_option("raven.ignore=\"undefined-variable\""),
            Some(LineSuppression::Codes(_))
        ));
    }

    #[test]
    fn in_chunk_ignore_chunk_directive_suppresses_body() {
        let src = "```{r}\n# raven: ignore-chunk\nx <- undefined\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert_eq!(meta.ignored_ranges.len(), 1);
        let r = &meta.ignored_ranges[0];
        // Body is lines 1..=2 (the directive line + the code line).
        assert_eq!((r.start, r.end), (1, 2));
        assert_eq!(r.what, LineSuppression::All);
    }

    #[test]
    fn in_chunk_ignore_chunk_with_code_selector() {
        let src =
            "```{r}\nx <- undefined  # nothing\n# raven: ignore-chunk[undefined-variable]\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert_eq!(meta.ignored_ranges.len(), 1);
        assert!(
            meta.ignored_ranges[0]
                .what
                .covers(Some("undefined-variable"))
        );
        assert!(!meta.ignored_ranges[0].what.covers(Some("line-length")));
    }

    #[test]
    fn plain_chunk_without_option_is_not_suppressed() {
        let src = "```{r}\nx <- 1\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert!(meta.ignored_ranges.is_empty());
        assert!(meta.suppression_directives.is_empty());
    }

    #[test]
    fn non_r_chunk_option_is_ignored() {
        let src = "```{python, raven.ignore=TRUE}\nprint('hi')\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert!(meta.ignored_ranges.is_empty());
    }

    #[test]
    fn chunk_option_suppression_crlf() {
        // CRLF line endings must not defeat header-option parsing or the
        // in-chunk directive (regex tolerates trailing \r).
        let src = "```{r, raven.ignore=TRUE}\r\nx <- undefined\r\n```\r\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert_eq!(meta.ignored_ranges.len(), 1);
        assert_eq!(meta.ignored_ranges[0].what, LineSuppression::All);

        let src2 = "```{r}\r\n# raven: ignore-chunk\r\nx <- undefined\r\n```\r\n";
        let mut meta2 = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta2, src2);
        assert_eq!(meta2.ignored_ranges.len(), 1);
    }

    #[test]
    fn chunk_option_suppression_bom_first_line_header() {
        // A BOM-prefixed document whose first line is the chunk header must
        // still parse the `raven.ignore` option.
        let src = "\u{FEFF}```{r, raven.ignore=TRUE}\nx <- undefined\n```\n";
        let mut meta = crate::cross_file::types::CrossFileMetadata::default();
        append_chunk_suppressions(&mut meta, src);
        assert_eq!(meta.ignored_ranges.len(), 1);
        assert_eq!(meta.ignored_ranges[0].what, LineSuppression::All);
    }

    // ---- detect_markdown_headings -----------------------------------------

    fn headings(text: &str) -> Vec<(u32, u32, String)> {
        detect_markdown_headings(text)
            .into_iter()
            .map(|h| (h.line, h.level, h.title))
            .collect()
    }

    #[test]
    fn markdown_headings_basic_levels() {
        let src = "# One\n\n## Two\n\n### Three\n";
        assert_eq!(
            headings(src),
            vec![
                (0, 1, "One".to_string()),
                (2, 2, "Two".to_string()),
                (4, 3, "Three".to_string()),
            ]
        );
    }

    #[test]
    fn markdown_headings_levels_one_through_six() {
        let src = "# a\n## b\n### c\n#### d\n##### e\n###### f\n####### g\n";
        // 7 hashes is not a heading.
        assert_eq!(
            headings(src),
            vec![
                (0, 1, "a".to_string()),
                (1, 2, "b".to_string()),
                (2, 3, "c".to_string()),
                (3, 4, "d".to_string()),
                (4, 5, "e".to_string()),
                (5, 6, "f".to_string()),
            ]
        );
    }

    #[test]
    fn markdown_headings_skip_yaml_frontmatter() {
        // The `# comment` and `## title` lines inside frontmatter are not headings.
        let src = "---\ntitle: Doc\n# yaml comment\n---\n\n# Real Heading\n";
        assert_eq!(headings(src), vec![(5, 1, "Real Heading".to_string())]);
    }

    #[test]
    fn markdown_headings_skip_unterminated_frontmatter_is_not_frontmatter() {
        // An unterminated leading `---` block is not valid frontmatter; the
        // heading after it is still detected (the `---`/`title:` lines are not
        // ATX headings, so they simply don't match).
        let src = "---\ntitle: Doc\n\n# Heading\n";
        assert_eq!(headings(src), vec![(3, 1, "Heading".to_string())]);
    }

    #[test]
    fn markdown_headings_skip_generic_fenced_code() {
        let src = "# Before\n\n```\n# not a heading\n```\n\n# After\n";
        assert_eq!(
            headings(src),
            vec![(0, 1, "Before".to_string()), (6, 1, "After".to_string()),]
        );
    }

    #[test]
    fn markdown_headings_skip_r_chunk_bodies() {
        let src = "# Before\n\n```{r}\n# an R comment\nx <- 1\n```\n\n## After\n";
        assert_eq!(
            headings(src),
            vec![(0, 1, "Before".to_string()), (7, 2, "After".to_string()),]
        );
    }

    #[test]
    fn markdown_headings_skip_tilde_fenced_code() {
        let src = "# Before\n\n~~~\n# not a heading\n~~~\n\n# After\n";
        assert_eq!(
            headings(src),
            vec![(0, 1, "Before".to_string()), (6, 1, "After".to_string()),]
        );
    }

    #[test]
    fn markdown_headings_unclosed_fence_runs_to_eof() {
        // The fence is never closed, so the `# heading` after it is code.
        let src = "# Before\n\n```\n# swallowed\n\n# also swallowed\n";
        assert_eq!(headings(src), vec![(0, 1, "Before".to_string())]);
    }

    #[test]
    fn markdown_headings_shorter_close_does_not_close_fence() {
        // Open fence of 4 backticks; a 3-backtick line does not close it, so
        // the `#` line stays inside the fence.
        let src = "````\n# inside\n```\n# still inside\n````\n# outside\n";
        assert_eq!(headings(src), vec![(5, 1, "outside".to_string())]);
    }

    #[test]
    fn markdown_headings_indented_close_does_not_close_fence() {
        // Fences are recognized only at column 0, matching the Rmd chunk
        // detector (`detect_rmd_chunks`). An indented ``` does not close the
        // fence, so the heading between it and the real close stays code.
        let src = "```\n# inside\n   ```\n# still inside\n```\n# outside\n";
        assert_eq!(headings(src), vec![(5, 1, "outside".to_string())]);
    }

    #[test]
    fn markdown_headings_reject_hashtag_without_space() {
        let src = "#hashtag\n# real\n";
        assert_eq!(headings(src), vec![(1, 1, "real".to_string())]);
    }

    #[test]
    fn markdown_headings_reject_indented_code_block() {
        // 4 spaces of indentation makes it an indented code block, not a heading.
        let src = "    # indented code\n  ## up to three spaces ok\n";
        assert_eq!(
            headings(src),
            vec![(1, 2, "up to three spaces ok".to_string())]
        );
    }

    #[test]
    fn markdown_headings_strip_closing_hash_sequence() {
        // `## foo ##` -> "foo" (closing sequence preceded by space).
        // `# bar#`    -> "bar#" (no space before '#', so it is content).
        let src = "## foo ##\n# bar#\n";
        assert_eq!(
            headings(src),
            vec![(0, 2, "foo".to_string()), (1, 1, "bar#".to_string()),]
        );
    }

    #[test]
    fn markdown_headings_drop_empty_titles() {
        // `#`, `###`, and `# ###` are all empty headings and are dropped.
        let src = "#\n###\n# ###\n# kept\n";
        assert_eq!(headings(src), vec![(3, 1, "kept".to_string())]);
    }

    #[test]
    fn markdown_headings_crlf() {
        let src = "# One\r\n\r\n## Two\r\n";
        assert_eq!(
            headings(src),
            vec![(0, 1, "One".to_string()), (2, 2, "Two".to_string()),]
        );
    }

    #[test]
    fn markdown_headings_bom_first_line() {
        let src = "\u{FEFF}# Title\n## Sub\n";
        assert_eq!(
            headings(src),
            vec![(0, 1, "Title".to_string()), (1, 2, "Sub".to_string()),]
        );
    }
}
