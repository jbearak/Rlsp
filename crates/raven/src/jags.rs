//! JAGS parsing and lightweight lexical support for editor assistance.
//!
//! The native Tree-sitter parser backs syntax diagnostics and structural
//! editor features. The recovery-oriented scanner remains deliberately
//! narrower than the parser: it identifies ASCII
//! JAGS call names, comment-free delimiters, distribution context after `~`,
//! and the active call argument. Completion, hover, and signature help can use
//! it without consulting the parse tree or any R-specific scope,
//! package, help, subprocess, or cross-file machinery.

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Parser, Tree};

use crate::foreign_syntax::{
    DelimiterIndex, DelimiterIndexBuilder, DelimiterKind, LexicalSpanKind,
};

/// Parse JAGS source while reusing an edited tree from the prior raw text.
pub(crate) fn parse_with_old_tree(text: &str, old_tree: Option<&Tree>) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_jags::LANGUAGE.into())
        .ok()?;
    parser.parse(text, old_tree)
}

fn special_operator_end_with_cancel_check(
    bytes: &[u8],
    start: usize,
    limit: usize,
    mut is_cancelled_at: impl FnMut(usize) -> bool,
) -> Result<Option<usize>, ()> {
    if bytes.get(start) != Some(&b'%') || start + 1 >= limit {
        return Ok(None);
    }
    if bytes[start + 1] == b'%' {
        return Ok(Some(start + 2));
    }

    let mut index = start + 1;
    while index < limit && !bytes[index].is_ascii_whitespace() {
        if is_cancelled_at(index) {
            return Err(());
        }
        if bytes[index] == b'%' {
            return Ok((index > start + 1).then_some(index + 1));
        }
        index += 1;
    }
    Ok(None)
}

fn special_operator_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    special_operator_end_with_cancel_check(bytes, start, limit, |_| false)
        .expect("non-cancelling special-operator scan")
}

/// Build bounded delimiter evidence using JAGS comment and `%...%` operator
/// rules. Quote-bearing spans remain deliberately opaque because JAGS does not
/// define string literals for Raven to interpret during parser recovery.
pub(crate) fn delimiter_index(
    text: &str,
    mut is_cancelled: impl FnMut() -> bool,
) -> Option<DelimiterIndex> {
    const CANCELLATION_INTERVAL: usize = 4 * 1024;

    let bytes = text.as_bytes();
    let mut builder = DelimiterIndexBuilder::new(bytes.len());
    let limit = builder.scan_limit();
    let mut index = 0usize;
    let mut next_cancellation_check = 0usize;

    while index < limit {
        if index >= next_cancellation_check {
            if is_cancelled() {
                return None;
            }
            next_cancellation_check = index.saturating_add(CANCELLATION_INTERVAL);
        }

        let special_operator_end =
            special_operator_end_with_cancel_check(bytes, index, limit, |position| {
                if position < next_cancellation_check {
                    return false;
                }
                next_cancellation_check = position.saturating_add(CANCELLATION_INTERVAL);
                is_cancelled()
            })
            .ok()?;
        if let Some(end) = special_operator_end {
            if !builder.push_span(index..end, LexicalSpanKind::Code) {
                break;
            }
            index = end;
            continue;
        }

        if bytes[index] == b'#' {
            let start = index;
            index += 1;
            while index < limit && !matches!(bytes[index], b'\r' | b'\n') {
                if index >= next_cancellation_check {
                    if is_cancelled() {
                        return None;
                    }
                    next_cancellation_check = index.saturating_add(CANCELLATION_INTERVAL);
                }
                index += 1;
            }
            if !builder.push_span(start..index, LexicalSpanKind::Trivia) {
                break;
            }
            continue;
        }

        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            let start = index;
            index += 2;
            let mut terminated = false;
            while index < limit {
                if index >= next_cancellation_check {
                    if is_cancelled() {
                        return None;
                    }
                    next_cancellation_check = index.saturating_add(CANCELLATION_INTERVAL);
                }
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    index += 2;
                    terminated = true;
                    break;
                }
                index += 1;
            }
            if !builder.push_span(
                start..index,
                if terminated {
                    LexicalSpanKind::Trivia
                } else {
                    LexicalSpanKind::AmbiguousCode
                },
            ) {
                break;
            }
            continue;
        }

        if matches!(bytes[index], b'"' | b'\'') {
            let start = index;
            let terminator = bytes[index];
            index += 1;
            while index < limit {
                if index >= next_cancellation_check {
                    if is_cancelled() {
                        return None;
                    }
                    next_cancellation_check = index.saturating_add(CANCELLATION_INTERVAL);
                }
                if bytes[index] == b'\\' && index + 1 < limit {
                    index += 2;
                } else {
                    let byte = bytes[index];
                    index += 1;
                    if byte == terminator {
                        break;
                    }
                }
            }
            if !builder.push_span(start..index, LexicalSpanKind::AmbiguousCode) {
                break;
            }
            continue;
        }

        let delimiter = match bytes[index] {
            b'(' => Some((DelimiterKind::Paren, true)),
            b')' => Some((DelimiterKind::Paren, false)),
            b'[' => Some((DelimiterKind::Bracket, true)),
            b']' => Some((DelimiterKind::Bracket, false)),
            b'{' => Some((DelimiterKind::Brace, true)),
            b'}' => Some((DelimiterKind::Brace, false)),
            _ => None,
        };
        if let Some((kind, is_opener)) = delimiter
            && !builder.push_event(kind, is_opener, index, index + 1)
        {
            break;
        }
        index += text[index..]
            .chars()
            .next()
            .expect("index is on a UTF-8 boundary")
            .len_utf8();
    }

    if is_cancelled() {
        return None;
    }
    Some(builder.finish(index))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Identifier(String),
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    Comma,
    Tilde,
    LeftArrow,
    Equals,
    Comparison,
    Semicolon,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    start_byte: usize,
    end_byte: usize,
    range: Range,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LexState {
    Code,
    LineComment,
    BlockComment,
    DoubleQuoted,
    SingleQuoted,
}

/// A call-site identifier under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JagsCallSite {
    /// Exact, case-sensitive identifier.
    pub name: String,
    /// UTF-16 range of the identifier.
    pub range: Range,
    /// Whether the nearest preceding code token is `~`.
    pub after_tilde: bool,
}

/// The innermost open JAGS call at the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JagsActiveCall {
    /// Exact, case-sensitive callee identifier.
    pub name: String,
    /// UTF-16 range of the callee identifier.
    pub range: Range,
    /// Whether the callee follows `~`.
    pub after_tilde: bool,
    /// Zero-based active argument, counting only commas at this call's depth.
    pub active_parameter: u32,
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic()
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_')
}

fn advance_ascii(byte: u8, index: &mut usize, line: &mut u32, column: &mut u32) {
    *index += 1;
    if byte == b'\n' {
        *line += 1;
        *column = 0;
    } else {
        *column += 1;
    }
}

fn advance_char(text: &str, index: &mut usize, line: &mut u32, column: &mut u32) {
    let byte = text.as_bytes()[*index];
    if byte.is_ascii() {
        advance_ascii(byte, index, line, column);
        return;
    }
    let character = text[*index..]
        .chars()
        .next()
        .expect("index is within text and on a UTF-8 boundary");
    *index += character.len_utf8();
    *column += character.len_utf16() as u32;
}

fn push_punctuation(
    tokens: &mut Vec<Token>,
    kind: TokenKind,
    index: &mut usize,
    line: &mut u32,
    column: &mut u32,
) {
    let start_byte = *index;
    let start = Position::new(*line, *column);
    advance_ascii(0, index, line, column);
    tokens.push(Token {
        kind,
        start_byte,
        end_byte: *index,
        range: Range::new(start, Position::new(*line, *column)),
    });
}

fn push_two_byte_punctuation(
    tokens: &mut Vec<Token>,
    kind: TokenKind,
    index: &mut usize,
    line: &mut u32,
    column: &mut u32,
) {
    let start_byte = *index;
    let start = Position::new(*line, *column);
    advance_ascii(0, index, line, column);
    advance_ascii(0, index, line, column);
    tokens.push(Token {
        kind,
        start_byte,
        end_byte: *index,
        range: Range::new(start, Position::new(*line, *column)),
    });
}

fn tokenize_until(text: &str, limit: usize) -> (Vec<Token>, LexState) {
    let bytes = text.as_bytes();
    let limit = limit.min(bytes.len());
    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut line = 0u32;
    let mut column = 0u32;
    let mut state = LexState::Code;

    while index < limit {
        match state {
            LexState::Code => {
                if let Some(end) = special_operator_end(bytes, index, limit) {
                    while index < end {
                        advance_char(text, &mut index, &mut line, &mut column);
                    }
                    continue;
                }
                if bytes[index] == b'#' {
                    state = LexState::LineComment;
                    advance_ascii(bytes[index], &mut index, &mut line, &mut column);
                    continue;
                }
                if bytes[index] == b'/' && index + 1 < limit && bytes[index + 1] == b'*' {
                    state = LexState::BlockComment;
                    advance_ascii(b'/', &mut index, &mut line, &mut column);
                    advance_ascii(b'*', &mut index, &mut line, &mut column);
                    continue;
                }
                if bytes[index] == b'"' {
                    state = LexState::DoubleQuoted;
                    advance_ascii(bytes[index], &mut index, &mut line, &mut column);
                    continue;
                }
                if bytes[index] == b'\'' {
                    state = LexState::SingleQuoted;
                    advance_ascii(bytes[index], &mut index, &mut line, &mut column);
                    continue;
                }
                if is_identifier_start(bytes[index]) {
                    let start_byte = index;
                    let start = Position::new(line, column);
                    while index < limit && is_identifier_continue(bytes[index]) {
                        advance_ascii(bytes[index], &mut index, &mut line, &mut column);
                    }
                    tokens.push(Token {
                        kind: TokenKind::Identifier(text[start_byte..index].to_string()),
                        start_byte,
                        end_byte: index,
                        range: Range::new(start, Position::new(line, column)),
                    });
                    continue;
                }

                if bytes[index] == b'<' && index + 1 < limit && bytes[index + 1] == b'-' {
                    push_two_byte_punctuation(
                        &mut tokens,
                        TokenKind::LeftArrow,
                        &mut index,
                        &mut line,
                        &mut column,
                    );
                    continue;
                }
                if index + 1 < limit
                    && matches!(
                        (bytes[index], bytes[index + 1]),
                        (b'<', b'=') | (b'>', b'=') | (b'=', b'=') | (b'!', b'=')
                    )
                {
                    push_two_byte_punctuation(
                        &mut tokens,
                        TokenKind::Comparison,
                        &mut index,
                        &mut line,
                        &mut column,
                    );
                    continue;
                }

                let kind = match bytes[index] {
                    b'(' => Some(TokenKind::LeftParen),
                    b')' => Some(TokenKind::RightParen),
                    b'[' => Some(TokenKind::LeftBracket),
                    b']' => Some(TokenKind::RightBracket),
                    b'{' => Some(TokenKind::LeftBrace),
                    b'}' => Some(TokenKind::RightBrace),
                    b',' => Some(TokenKind::Comma),
                    b'~' => Some(TokenKind::Tilde),
                    b'=' => Some(TokenKind::Equals),
                    b';' => Some(TokenKind::Semicolon),
                    _ => None,
                };
                if let Some(kind) = kind {
                    push_punctuation(&mut tokens, kind, &mut index, &mut line, &mut column);
                } else {
                    advance_char(text, &mut index, &mut line, &mut column);
                }
            }
            LexState::LineComment => {
                let byte = bytes[index];
                advance_ascii(byte, &mut index, &mut line, &mut column);
                if byte == b'\n' {
                    state = LexState::Code;
                }
            }
            LexState::BlockComment => {
                if bytes[index] == b'*' && index + 1 < limit && bytes[index + 1] == b'/' {
                    advance_ascii(b'*', &mut index, &mut line, &mut column);
                    advance_ascii(b'/', &mut index, &mut line, &mut column);
                    state = LexState::Code;
                } else {
                    advance_char(text, &mut index, &mut line, &mut column);
                }
            }
            LexState::DoubleQuoted | LexState::SingleQuoted => {
                let terminator = if state == LexState::DoubleQuoted {
                    b'"'
                } else {
                    b'\''
                };
                if bytes[index] == b'\\' && index + 1 < limit {
                    advance_ascii(b'\\', &mut index, &mut line, &mut column);
                    advance_char(text, &mut index, &mut line, &mut column);
                } else {
                    let byte = bytes[index];
                    advance_char(text, &mut index, &mut line, &mut column);
                    if byte == terminator {
                        state = LexState::Code;
                    }
                }
            }
        }
    }

    (tokens, state)
}

fn tokenize(text: &str) -> Vec<Token> {
    tokenize_until(text, text.len()).0
}

/// Returns whether the raw span contains only whitespace and comments.
///
/// JAGS comments are lexical whitespace, so a terminated block comment or a
/// line comment followed by a newline may separate a callee from `(`. Any
/// operator, assignment, literal, semicolon, quoted fragment, or other code is
/// a barrier even when the lightweight token stream does not otherwise retain
/// it.
fn is_trivia_span(text: &str, start: usize, end: usize) -> bool {
    let bytes = text.as_bytes();
    let mut index = start;
    let mut state = LexState::Code;

    while index < end {
        match state {
            LexState::Code => {
                if bytes[index].is_ascii_whitespace() {
                    index += 1;
                } else if bytes[index] == b'#' {
                    state = LexState::LineComment;
                    index += 1;
                } else if bytes[index] == b'/' && index + 1 < end && bytes[index + 1] == b'*' {
                    state = LexState::BlockComment;
                    index += 2;
                } else if !bytes[index].is_ascii() {
                    let character = text[index..end]
                        .chars()
                        .next()
                        .expect("index is on a UTF-8 boundary within the span");
                    if !character.is_whitespace() {
                        return false;
                    }
                    index += character.len_utf8();
                } else {
                    return false;
                }
            }
            LexState::LineComment => {
                if bytes[index] == b'\n' {
                    state = LexState::Code;
                }
                index += 1;
            }
            LexState::BlockComment => {
                if bytes[index] == b'*' && index + 1 < end && bytes[index + 1] == b'/' {
                    state = LexState::Code;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            LexState::DoubleQuoted | LexState::SingleQuoted => return false,
        }
    }

    state == LexState::Code
}

fn token_follows_with_trivia(text: &str, previous: &Token, next: &Token) -> bool {
    is_trivia_span(text, previous.end_byte, next.start_byte)
}

fn identifier_follows_tilde(text: &str, tokens: &[Token], index: usize) -> bool {
    index > 0
        && matches!(tokens[index - 1].kind, TokenKind::Tilde)
        && token_follows_with_trivia(text, &tokens[index - 1], &tokens[index])
}

fn byte_offset_at_position(text: &str, position: Position) -> usize {
    let mut line_start = 0usize;
    let mut current_line = 0u32;
    while current_line < position.line {
        let Some(relative_newline) = text[line_start..].find('\n') else {
            return text.len();
        };
        line_start += relative_newline + 1;
        current_line += 1;
    }
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |offset| line_start + offset);
    let line_text = &text[line_start..line_end];
    line_start + crate::utf16::utf16_column_to_byte_offset(line_text, position.character)
}

/// Returns a catalog-eligible call identifier under `position`.
///
/// Whitespace and comments may separate the name from `(` and from `~`.
pub fn call_site_at_position(text: &str, position: Position) -> Option<JagsCallSite> {
    let cursor = byte_offset_at_position(text, position);
    let tokens = tokenize(text);
    let index = tokens.iter().position(|token| {
        token.start_byte <= cursor
            && cursor < token.end_byte
            && matches!(token.kind, TokenKind::Identifier(_))
    })?;
    let TokenKind::Identifier(name) = &tokens[index].kind else {
        return None;
    };
    let left_paren = tokens.get(index + 1)?;
    if !matches!(left_paren.kind, TokenKind::LeftParen)
        || !token_follows_with_trivia(text, &tokens[index], left_paren)
    {
        return None;
    }
    Some(JagsCallSite {
        name: name.clone(),
        range: tokens[index].range,
        after_tilde: identifier_follows_tilde(text, &tokens, index),
    })
}

fn identifier_is_for_iterator_separator(text: &str, tokens: &[Token], index: usize) -> bool {
    let Some(previous) = index.checked_sub(1).and_then(|i| tokens.get(i)) else {
        return false;
    };
    if !matches!(previous.kind, TokenKind::Identifier(_))
        || !token_follows_with_trivia(text, previous, &tokens[index])
    {
        return false;
    }

    let mut parenthesis_depth = 0usize;
    for open_index in (0..index).rev() {
        match tokens[open_index].kind {
            TokenKind::RightParen => parenthesis_depth += 1,
            TokenKind::LeftParen if parenthesis_depth == 0 => {
                let Some(keyword_index) = open_index.checked_sub(1) else {
                    return false;
                };
                return matches!(
                    &tokens[keyword_index].kind,
                    TokenKind::Identifier(name) if name == "for"
                ) && token_follows_with_trivia(
                    text,
                    &tokens[keyword_index],
                    &tokens[open_index],
                );
            }
            TokenKind::LeftParen => parenthesis_depth -= 1,
            _ => {}
        }
    }
    false
}

/// Returns whether the identifier under `position` is acting as JAGS syntax.
///
/// This recognizes block introducers, loop syntax, `var` declarations, and
/// call-shaped `T`/`I` bounds. Plain identifiers named `T` or `I` are not
/// syntax and remain eligible for file-local navigation.
pub fn syntax_site_at_position(text: &str, position: Position) -> bool {
    let cursor = byte_offset_at_position(text, position);
    let tokens = tokenize(text);
    let Some(index) = tokens.iter().position(|token| {
        token.start_byte <= cursor
            && cursor < token.end_byte
            && matches!(token.kind, TokenKind::Identifier(_))
    }) else {
        return false;
    };
    let TokenKind::Identifier(name) = &tokens[index].kind else {
        return false;
    };

    let next_with_trivia = |kind: TokenKind| {
        tokens.get(index + 1).is_some_and(|next| {
            next.kind == kind && token_follows_with_trivia(text, &tokens[index], next)
        })
    };

    match name.as_str() {
        "data" | "model" => next_with_trivia(TokenKind::LeftBrace),
        "for" | "T" | "I" => next_with_trivia(TokenKind::LeftParen),
        "var" => tokens.get(index + 1).is_some_and(|next| {
            matches!(next.kind, TokenKind::Identifier(_))
                && token_follows_with_trivia(text, &tokens[index], next)
        }),
        "in" => identifier_is_for_iterator_separator(text, &tokens, index),
        _ => false,
    }
}

#[derive(Clone, Debug)]
enum Delimiter {
    Call(JagsActiveCall),
    Parenthesized,
    Bracket,
    Brace,
}

/// Returns the innermost open call containing `position`.
pub fn active_call_at_position(text: &str, position: Position) -> Option<JagsActiveCall> {
    let cursor = byte_offset_at_position(text, position);
    let (tokens, state) = tokenize_until(text, cursor);
    if state != LexState::Code {
        return None;
    }
    let mut delimiters = Vec::new();

    for (index, token) in tokens.iter().enumerate() {
        if token.start_byte >= cursor {
            break;
        }
        match token.kind {
            TokenKind::LeftParen => {
                if let Some(Token {
                    kind: TokenKind::Identifier(name),
                    range,
                    ..
                }) = index
                    .checked_sub(1)
                    .and_then(|previous| tokens.get(previous))
                    .filter(|previous| token_follows_with_trivia(text, previous, token))
                {
                    let after_tilde = identifier_follows_tilde(text, &tokens, index - 1);
                    delimiters.push(Delimiter::Call(JagsActiveCall {
                        name: name.clone(),
                        range: *range,
                        after_tilde,
                        active_parameter: 0,
                    }));
                } else {
                    delimiters.push(Delimiter::Parenthesized);
                }
            }
            TokenKind::LeftBracket => delimiters.push(Delimiter::Bracket),
            TokenKind::LeftBrace => delimiters.push(Delimiter::Brace),
            TokenKind::RightParen => {
                if matches!(
                    delimiters.last(),
                    Some(Delimiter::Call(_) | Delimiter::Parenthesized)
                ) {
                    delimiters.pop();
                }
            }
            TokenKind::RightBracket => {
                if matches!(delimiters.last(), Some(Delimiter::Bracket)) {
                    delimiters.pop();
                }
            }
            TokenKind::RightBrace => {
                if matches!(delimiters.last(), Some(Delimiter::Brace)) {
                    delimiters.pop();
                }
            }
            TokenKind::Comma => {
                if let Some(Delimiter::Call(call)) = delimiters.last_mut() {
                    call.active_parameter += 1;
                }
            }
            TokenKind::Identifier(_)
            | TokenKind::Tilde
            | TokenKind::LeftArrow
            | TokenKind::Equals
            | TokenKind::Comparison
            | TokenKind::Semicolon => {}
        }
    }

    delimiters
        .into_iter()
        .rev()
        .find_map(|delimiter| match delimiter {
            Delimiter::Call(call) => Some(call),
            Delimiter::Parenthesized | Delimiter::Bracket | Delimiter::Brace => None,
        })
}

/// A relation identifier recovered lexically when the native parser cannot
/// form a relation node around an incomplete or out-of-order buffer fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JagsRecoveryRelation {
    pub(crate) name: String,
    pub(crate) range: Range,
    pub(crate) selection_range: Range,
}

fn position_at_byte_offset(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let prefix = &text[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    Position::new(line, text[line_start..offset].encode_utf16().count() as u32)
}

/// Scan the malformed relation's RHS to its own lexical boundary.
///
/// A top-level semicolon belongs to the relation. A top-level newline, closing
/// brace, or opening brace starts another construct and does not. Comments and
/// quoted text are opaque, and delimiters inside calls/subsets do not end the
/// relation. `last_code_end` excludes trailing whitespace and comments.
fn recovery_relation_end_byte(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut index = start;
    let mut last_code_end = start;
    let mut parentheses = 0usize;
    let mut brackets = 0usize;
    let mut state = LexState::Code;

    while index < bytes.len() {
        match state {
            LexState::Code => {
                let byte = bytes[index];
                let top_level = parentheses == 0 && brackets == 0;
                if let Some(end) = special_operator_end(bytes, index, bytes.len()) {
                    last_code_end = end;
                    index = end;
                } else if byte == b'#' {
                    state = LexState::LineComment;
                    index += 1;
                } else if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
                    state = LexState::BlockComment;
                    index += 2;
                } else if byte == b'"' {
                    state = LexState::DoubleQuoted;
                    last_code_end = index + 1;
                    index += 1;
                } else if byte == b'\'' {
                    state = LexState::SingleQuoted;
                    last_code_end = index + 1;
                    index += 1;
                } else if top_level && byte == b';' {
                    return index + 1;
                } else if top_level && matches!(byte, b'\n' | b'{' | b'}') {
                    return last_code_end;
                } else {
                    match byte {
                        b'(' => parentheses += 1,
                        b')' => parentheses = parentheses.saturating_sub(1),
                        b'[' => brackets += 1,
                        b']' => brackets = brackets.saturating_sub(1),
                        _ => {}
                    }
                    if !byte.is_ascii_whitespace() {
                        last_code_end = index + 1;
                    }
                    index += 1;
                }
            }
            LexState::LineComment => {
                if bytes[index] == b'\n' {
                    if parentheses == 0 && brackets == 0 {
                        return last_code_end;
                    }
                    state = LexState::Code;
                }
                index += 1;
            }
            LexState::BlockComment => {
                if bytes[index] == b'*' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
                    state = LexState::Code;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            LexState::DoubleQuoted | LexState::SingleQuoted => {
                let terminator = if state == LexState::DoubleQuoted {
                    b'"'
                } else {
                    b'\''
                };
                if bytes[index] == b'\\' && index + 1 < bytes.len() {
                    index += 2;
                    last_code_end = index;
                } else {
                    let byte = bytes[index];
                    index += 1;
                    last_code_end = index;
                    if byte == terminator {
                        state = LexState::Code;
                    }
                }
            }
        }
    }

    last_code_end
}

/// Recover plain/subset relation identifiers using the editor scanner's
/// comment- and string-aware token stream.
///
/// Native relation nodes remain authoritative for valid JAGS. This narrow
/// fallback preserves outline symbols in malformed buffers (for example, a
/// relation temporarily preceding `model`) without ever interpreting relation
/// text inside `#`, `/* ... */`, or quoted content.
pub(crate) fn recovery_relation_identifiers(text: &str) -> Vec<JagsRecoveryRelation> {
    let tokens = tokenize(text);
    let mut relations = Vec::new();

    for (index, token) in tokens.iter().enumerate() {
        let TokenKind::Identifier(name) = &token.kind else {
            continue;
        };
        let mut next = index + 1;
        let mut lhs_end = token;
        while tokens.get(next).is_some_and(|candidate| {
            matches!(candidate.kind, TokenKind::LeftBracket)
                && token_follows_with_trivia(text, lhs_end, candidate)
        }) {
            let mut depth = 0usize;
            let mut closing = None;
            while let Some(candidate) = tokens.get(next) {
                match candidate.kind {
                    TokenKind::LeftBracket => depth += 1,
                    TokenKind::RightBracket => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            closing = Some(candidate);
                            next += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                next += 1;
            }
            if depth != 0 {
                continue;
            }
            lhs_end = closing.expect("balanced subset has a closing bracket");
        }

        if let Some(operator) = tokens.get(next).filter(|candidate| {
            matches!(
                candidate.kind,
                TokenKind::LeftArrow | TokenKind::Equals | TokenKind::Tilde
            ) && token_follows_with_trivia(text, lhs_end, candidate)
        }) {
            let end_byte = recovery_relation_end_byte(text, operator.end_byte);
            relations.push(JagsRecoveryRelation {
                name: name.clone(),
                range: Range::new(token.range.start, position_at_byte_offset(text, end_byte)),
                selection_range: token.range,
            });
        }
    }

    relations
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_relations_are_comment_aware_and_keep_utf16_identifier_ranges() {
        let code = concat!(
            "# ghost <- 1\n",
            "/* hidden ~ dnorm(0, 1)\n",
            "   still_hidden = 2 */\n",
            "\"quoted <- 3\"\n",
            "model { /* 💥 */ x[i] <- 1; y ~ dnorm(0, 1) }\n",
        );

        assert_eq!(
            recovery_relation_identifiers(code),
            vec![
                JagsRecoveryRelation {
                    name: "x".to_string(),
                    range: Range::new(Position::new(4, 17), Position::new(4, 27)),
                    selection_range: Range::new(Position::new(4, 17), Position::new(4, 18)),
                },
                JagsRecoveryRelation {
                    name: "y".to_string(),
                    range: Range::new(Position::new(4, 28), Position::new(4, 43)),
                    selection_range: Range::new(Position::new(4, 28), Position::new(4, 29)),
                },
            ]
        );
    }

    #[test]
    fn recovery_relations_require_raw_trivia_adjacency_and_skip_comparisons() {
        let code = concat!(
            "a <= b; c >= d; e == f; g != h\n",
            "plus + hidden <- 1\n",
            "call() = 2; discarded + 1 = 2\n",
            "x /* trivia */ <- 1; y[i] # line trivia\n",
            "  ~ dnorm(0, 1)\n",
        );

        assert_eq!(
            recovery_relation_identifiers(code),
            vec![
                JagsRecoveryRelation {
                    name: "hidden".to_string(),
                    range: Range::new(Position::new(1, 7), Position::new(1, 18)),
                    selection_range: Range::new(Position::new(1, 7), Position::new(1, 13)),
                },
                JagsRecoveryRelation {
                    name: "x".to_string(),
                    range: Range::new(Position::new(3, 0), Position::new(3, 20)),
                    selection_range: Range::new(Position::new(3, 0), Position::new(3, 1)),
                },
                JagsRecoveryRelation {
                    name: "y".to_string(),
                    range: Range::new(Position::new(3, 21), Position::new(4, 15)),
                    selection_range: Range::new(Position::new(3, 21), Position::new(3, 22)),
                },
            ]
        );
    }

    #[test]
    fn call_sites_are_distribution_aware_and_support_dotted_names() {
        let code = "model {\r\n  y ~ dmnorm.vcov /* gap */ (mu, sigma)\r\n}";
        let site = call_site_at_position(code, Position::new(1, 8)).unwrap();
        assert_eq!(site.name, "dmnorm.vcov");
        assert!(site.after_tilde);
        assert_eq!(
            site.range,
            Range::new(Position::new(1, 6), Position::new(1, 17))
        );
    }

    #[test]
    fn comments_and_quoted_noise_do_not_create_call_sites() {
        for (code, position) in [
            ("# dnorm(0, 1)", Position::new(0, 3)),
            ("/* dnorm(0, 1) */", Position::new(0, 4)),
            ("\"dnorm(0, 1)\"", Position::new(0, 2)),
            ("'dnorm(0, 1)'", Position::new(0, 2)),
        ] {
            assert!(call_site_at_position(code, position).is_none(), "{code}");
        }
    }

    #[test]
    fn plain_identifiers_and_unknown_spacing_without_call_are_inert() {
        assert!(call_site_at_position("dnorm + 1", Position::new(0, 2)).is_none());
        assert!(call_site_at_position("dnorm /* unfinished", Position::new(0, 2)).is_none());
    }

    #[test]
    fn discarded_code_between_identifier_and_group_is_a_call_barrier() {
        for code in [
            "sqrt + (1)",
            "sqrt <- (1)",
            "sqrt = (1)",
            "sqrt; (1)",
            "sqrt 1 (2)",
        ] {
            assert!(
                call_site_at_position(code, Position::new(0, 2)).is_none(),
                "call site: {code}"
            );
            assert!(
                active_call_at_position(code, Position::new(0, code.len() as u32 - 1)).is_none(),
                "active call: {code}"
            );
        }
    }

    #[test]
    fn whitespace_and_completed_comments_may_separate_a_real_call() {
        for (code, cursor) in [
            ("sqrt /* comment */ (1)", Position::new(0, 21)),
            ("sqrt # comment\n (1)", Position::new(1, 2)),
        ] {
            assert_eq!(
                call_site_at_position(code, Position::new(0, 2))
                    .unwrap()
                    .name,
                "sqrt"
            );
            assert_eq!(active_call_at_position(code, cursor).unwrap().name, "sqrt");
        }
    }

    #[test]
    fn syntax_sites_distinguish_plain_contextual_names() {
        assert!(syntax_site_at_position(
            "x ~ dnorm(0, 1) T(0,)",
            Position::new(0, 16)
        ));
        assert!(syntax_site_at_position(
            "for (i in 1:N)",
            Position::new(0, 7)
        ));
        assert!(!syntax_site_at_position("T + I", Position::new(0, 0)));
        assert!(!syntax_site_at_position("T + I", Position::new(0, 4)));
    }

    #[test]
    fn active_argument_ignores_nested_calls_and_index_commas() {
        let code = "outer(x[1, 2], inner(a, b), )";
        let outer = active_call_at_position(code, Position::new(0, 28)).unwrap();
        assert_eq!(outer.name, "outer");
        assert_eq!(outer.active_parameter, 2);

        let inner = active_call_at_position(code, Position::new(0, 24)).unwrap();
        assert_eq!(inner.name, "inner");
        assert_eq!(inner.active_parameter, 1);
    }

    #[test]
    fn active_argument_handles_multiline_comments_and_unicode() {
        let code = "😀 dnorm(\n  0, # first\n  sqrt(2),\n  3\n)";
        let call = active_call_at_position(code, Position::new(3, 3)).unwrap();
        assert_eq!(call.name, "dnorm");
        assert_eq!(call.active_parameter, 2);
        assert_eq!(call.range.start, Position::new(0, 3));
    }

    #[test]
    fn scanner_work_is_linear_on_large_input() {
        let mut code = String::from("model {\n");
        for index in 0..4_000 {
            code.push_str(&format!("x{index} ~ dnorm(0, 1) # comment\n"));
        }
        code.push('}');
        let sites = tokenize(&code);
        assert!(sites.len() < code.len());
        let last_line = 4_000;
        let site = call_site_at_position(&code, Position::new(last_line, 10)).unwrap();
        assert_eq!(site.name, "dnorm");
    }

    #[test]
    fn delimiter_index_cancels_inside_long_opaque_tokens() {
        let payload = "x".repeat(16 * 1024);
        let sources = [
            format!("#{payload}"),
            format!("%{payload}%"),
            format!("\"{payload}\""),
            format!("'{payload}'"),
            format!("/*{payload}*/"),
        ];

        for source in sources {
            let mut checks = 0usize;
            let index = delimiter_index(&source, || {
                checks += 1;
                checks == 3
            });
            assert!(
                index.is_none(),
                "scan did not cancel for token: {}",
                &source[..1]
            );
            assert_eq!(checks, 3, "unexpected cancellation cadence");
        }
    }
}
