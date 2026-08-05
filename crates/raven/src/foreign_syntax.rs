//! Shared bounded lexical evidence for native Stan and JAGS syntax diagnostics.
//!
//! The language modules own their lexical rules and feed this index only
//! delimiter events plus spans whose contents must not be interpreted as
//! delimiters. Diagnostics may trust a recovery window only when the scan
//! covered it and no language-specific opaque span intersects it.

use std::ops::Range;

const MAX_SCANNED_BYTES: usize = 4 * 1024 * 1024;
const MAX_DELIMITER_EVENTS: usize = 65_536;
const MAX_LEXICAL_SPANS: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DelimiterKind {
    Paren,
    Bracket,
    Brace,
}

impl DelimiterKind {
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Paren => 0,
            Self::Bracket => 1,
            Self::Brace => 2,
        }
    }

    pub(crate) fn opener(self) -> &'static str {
        match self {
            Self::Paren => "(",
            Self::Bracket => "[",
            Self::Brace => "{",
        }
    }

    pub(crate) fn closer(self) -> &'static str {
        match self {
            Self::Paren => ")",
            Self::Bracket => "]",
            Self::Brace => "}",
        }
    }

    pub(crate) fn from_opener(text: &str) -> Option<Self> {
        match text {
            "(" => Some(Self::Paren),
            "[" => Some(Self::Bracket),
            "{" => Some(Self::Brace),
            _ => None,
        }
    }

    pub(crate) fn from_closer(text: &str) -> Option<Self> {
        match text {
            ")" => Some(Self::Paren),
            "]" => Some(Self::Bracket),
            "}" => Some(Self::Brace),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DelimiterEvent {
    pub(crate) kind: DelimiterKind,
    pub(crate) is_opener: bool,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LexicalSpanKind {
    /// Comments are ignored both for delimiter matching and range endings.
    Trivia,
    /// Valid source whose internal delimiters are ignored but whose extent is
    /// meaningful code (for example, a Stan string or `#include` line).
    Code,
    /// Source that prevents a trustworthy delimiter explanation in any
    /// intersecting recovery window.
    AmbiguousCode,
}

#[derive(Clone, Debug)]
struct LexicalSpan {
    range: Range<usize>,
    kind: LexicalSpanKind,
}

#[derive(Clone, Debug)]
pub(crate) struct DelimiterIndex {
    events: Vec<DelimiterEvent>,
    spans: Vec<LexicalSpan>,
    ambiguous_spans: Vec<Range<usize>>,
    scanned_to: usize,
}

impl DelimiterIndex {
    pub(crate) fn is_reliable(&self, range: Range<usize>) -> bool {
        if range.end > self.scanned_to {
            return false;
        }

        let first_overlap = self
            .ambiguous_spans
            .partition_point(|span| span.end <= range.start);
        self.ambiguous_spans
            .get(first_overlap)
            .is_none_or(|span| span.start >= range.end)
    }

    pub(crate) fn events_in(&self, range: Range<usize>) -> &[DelimiterEvent] {
        let start = self
            .events
            .partition_point(|event| event.end_byte <= range.start);
        let end = self
            .events
            .partition_point(|event| event.start_byte < range.end);
        &self.events[start..end]
    }

    /// Return the final meaningful byte on one source line after `start`.
    /// Trailing comments and a comment followed only by an enclosing closer do
    /// not extend an unclosed-opener diagnostic range.
    pub(crate) fn meaningful_end(&self, text: &str, start: usize, end: usize) -> Option<usize> {
        if end > self.scanned_to || start > end || end > text.len() {
            return None;
        }

        let mut meaningful_end = start;
        let mut index = start;
        let mut span_index = self.spans.partition_point(|span| span.range.end <= start);

        while index < end {
            let span = self
                .spans
                .get(span_index)
                .filter(|span| span.range.start < end && span.range.end > index);
            if let Some(span) = span {
                if index < span.range.start {
                    meaningful_end = meaningful_end.max(last_non_whitespace_end(
                        text,
                        index,
                        span.range.start.min(end),
                    ));
                    index = span.range.start;
                    continue;
                }

                let span_end = span.range.end.min(end);
                match span.kind {
                    LexicalSpanKind::Trivia => {
                        let trailing = text.get(span_end..end)?.trim_start();
                        if trailing.is_empty()
                            || trailing
                                .get(..1)
                                .is_some_and(|token| DelimiterKind::from_closer(token).is_some())
                        {
                            return Some(meaningful_end);
                        }
                    }
                    LexicalSpanKind::Code | LexicalSpanKind::AmbiguousCode => {
                        meaningful_end = meaningful_end.max(span_end);
                    }
                }
                index = span_end;
                span_index += 1;
                continue;
            }

            meaningful_end = meaningful_end.max(last_non_whitespace_end(text, index, end));
            break;
        }

        Some(meaningful_end)
    }
}

fn last_non_whitespace_end(text: &str, start: usize, end: usize) -> usize {
    text.get(start..end)
        .and_then(|segment| {
            segment
                .char_indices()
                .rev()
                .find(|(_, character)| !character.is_whitespace())
                .map(|(offset, character)| start + offset + character.len_utf8())
        })
        .unwrap_or(start)
}

pub(crate) struct DelimiterIndexBuilder {
    source_len: usize,
    events: Vec<DelimiterEvent>,
    spans: Vec<LexicalSpan>,
    ambiguous_spans: Vec<Range<usize>>,
    exhausted_at: Option<usize>,
}

impl DelimiterIndexBuilder {
    pub(crate) fn new(source_len: usize) -> Self {
        Self {
            source_len,
            events: Vec::new(),
            spans: Vec::new(),
            ambiguous_spans: Vec::new(),
            exhausted_at: None,
        }
    }

    pub(crate) fn scan_limit(&self) -> usize {
        self.source_len.min(MAX_SCANNED_BYTES)
    }

    pub(crate) fn push_event(
        &mut self,
        kind: DelimiterKind,
        is_opener: bool,
        start_byte: usize,
        end_byte: usize,
    ) -> bool {
        if self.events.len() == MAX_DELIMITER_EVENTS {
            self.exhausted_at = Some(start_byte);
            return false;
        }
        self.events.push(DelimiterEvent {
            kind,
            is_opener,
            start_byte,
            end_byte,
        });
        true
    }

    pub(crate) fn push_span(&mut self, range: Range<usize>, kind: LexicalSpanKind) -> bool {
        if range.is_empty() {
            return true;
        }
        if self.spans.len() == MAX_LEXICAL_SPANS {
            self.exhausted_at = Some(range.start);
            return false;
        }
        debug_assert!(
            self.spans
                .last()
                .is_none_or(|previous| previous.range.end <= range.start)
        );
        if kind == LexicalSpanKind::AmbiguousCode {
            self.ambiguous_spans.push(range.clone());
        }
        self.spans.push(LexicalSpan { range, kind });
        true
    }

    pub(crate) fn finish(self, scanned_to: usize) -> DelimiterIndex {
        let scan_limit = self.source_len.min(MAX_SCANNED_BYTES);
        DelimiterIndex {
            events: self.events,
            spans: self.spans,
            ambiguous_spans: self.ambiguous_spans,
            scanned_to: self.exhausted_at.unwrap_or(scanned_to.min(scan_limit)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_byte_limit_marks_only_the_scanned_prefix_reliable() {
        let source_len = MAX_SCANNED_BYTES + 1;
        let index = DelimiterIndexBuilder::new(source_len).finish(source_len);

        assert!(index.is_reliable(0..MAX_SCANNED_BYTES));
        assert!(!index.is_reliable(MAX_SCANNED_BYTES..source_len));
    }

    #[test]
    fn delimiter_event_limit_stops_reliability_at_the_first_rejected_event() {
        let source_len = MAX_DELIMITER_EVENTS + 1;
        let mut builder = DelimiterIndexBuilder::new(source_len);
        for offset in 0..MAX_DELIMITER_EVENTS {
            assert!(builder.push_event(DelimiterKind::Paren, true, offset, offset + 1));
        }
        assert!(!builder.push_event(DelimiterKind::Paren, true, MAX_DELIMITER_EVENTS, source_len,));
        let index = builder.finish(source_len);

        assert!(index.is_reliable(0..MAX_DELIMITER_EVENTS));
        assert!(!index.is_reliable(MAX_DELIMITER_EVENTS..source_len));
        assert_eq!(index.events_in(0..source_len).len(), MAX_DELIMITER_EVENTS);
    }

    #[test]
    fn lexical_span_limit_stops_reliability_at_the_first_rejected_span() {
        let source_len = MAX_LEXICAL_SPANS + 1;
        let mut builder = DelimiterIndexBuilder::new(source_len);
        for offset in 0..MAX_LEXICAL_SPANS {
            assert!(builder.push_span(offset..offset + 1, LexicalSpanKind::Trivia));
        }
        assert!(!builder.push_span(MAX_LEXICAL_SPANS..source_len, LexicalSpanKind::Trivia,));
        let index = builder.finish(source_len);

        assert!(index.is_reliable(0..MAX_LEXICAL_SPANS));
        assert!(!index.is_reliable(MAX_LEXICAL_SPANS..source_len));
    }

    #[test]
    fn lexical_span_kinds_control_reliability_and_meaningful_end() {
        let trivia = "x // comment with }";
        let mut trivia_builder = DelimiterIndexBuilder::new(trivia.len());
        trivia_builder.push_span(2..trivia.len(), LexicalSpanKind::Trivia);
        let trivia_index = trivia_builder.finish(trivia.len());
        assert!(trivia_index.is_reliable(0..trivia.len()));
        assert_eq!(
            trivia_index.meaningful_end(trivia, 0, trivia.len()),
            Some(1)
        );

        let code = "#include <fake[delimiter]>";
        let mut code_builder = DelimiterIndexBuilder::new(code.len());
        code_builder.push_span(0..code.len(), LexicalSpanKind::Code);
        let code_index = code_builder.finish(code.len());
        assert!(code_index.is_reliable(0..code.len()));
        assert!(code_index.events_in(0..code.len()).is_empty());
        assert_eq!(
            code_index.meaningful_end(code, 0, code.len()),
            Some(code.len())
        );

        let ambiguous = "\"unterminated (";
        let mut ambiguous_builder = DelimiterIndexBuilder::new(ambiguous.len());
        ambiguous_builder.push_span(0..ambiguous.len(), LexicalSpanKind::AmbiguousCode);
        let ambiguous_index = ambiguous_builder.finish(ambiguous.len());
        assert!(!ambiguous_index.is_reliable(0..ambiguous.len()));
        assert_eq!(
            ambiguous_index.meaningful_end(ambiguous, 0, ambiguous.len()),
            Some(ambiguous.len())
        );
    }
}
