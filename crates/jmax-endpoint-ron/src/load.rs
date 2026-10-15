//! Bounded, schema-independent parsing of the supported RON configuration grammar.
//!
//! Parsing uses an explicit stack. A root has depth one; a variant and its
//! parenthesized payload each occupy one depth level and one node. Comment
//! nesting has its own depth counter, subject to the same configured ceiling.

use alloc::{boxed::Box, collections::BTreeSet, string::String, vec::Vec};

use crate::{
    ConfigLimits, ConfigNode, ConfigValue, Error, ErrorCode, LoadedConfig, Result,
    config_error, validate_input_size,
};
#[cfg(feature = "std")]
use crate::{EndpointModel, LoadOptions};

/// Parse exactly one value, retaining byte spans into `input` and a diagnostic
/// source label. The label is never interpreted as a filesystem path.
pub fn parse_str(input: &str, source_name: &str, limits: &ConfigLimits) -> Result<LoadedConfig> {
    validate_input_size(input, limits)?;
    check_string_size(source_name.len(), limits, 0)?;

    let mut parser = Parser { input, limits, pos: 0, nodes: 0 };
    let token = parser.value_token(1)?;
    let mut step = parser.start_value(token, 1)?;
    let mut frames: Vec<Frame> = Vec::new();

    loop {
        match step {
            Step::Value(node) => {
                if let Some(parent) = frames.last_mut() {
                    parent.accept(node)?;
                } else {
                    parser.skip_trivia()?;
                    if parser.pos != input.len() {
                        return Err(error_at(ErrorCode::TrailingContent, parser.pos,
                            "content follows the root configuration value"));
                    }
                    let mut label = String::new();
                    reserve_string(&mut label, source_name.len(), 0)?;
                    label.push_str(source_name);
                    return Ok(LoadedConfig { source_name: label, root: node });
                }
            }
            Step::Open(frame) => {
                frames.try_reserve(1).map_err(|_| error_at(ErrorCode::BudgetExhausted,
                    frame.start, "cannot allocate parser frame"))?;
                frames.push(frame);
            }
        }

        // Each iteration either consumes punctuation, opens a checked frame,
        // or attaches a completed node. There is no recursive parser call.
        let frame = frames.last_mut().ok_or_else(|| error_at(ErrorCode::InternalError,
            parser.pos, "parser has no pending collection"))?;
        match frame.next(&mut parser)? {
            FrameAction::Value(token, depth) => step = parser.start_value(token, depth)?,
            FrameAction::Close => {
                let frame = frames.pop().ok_or_else(|| error_at(ErrorCode::InternalError,
                    parser.pos, "parser collection stack is empty"))?;
                step = Step::Value(frame.finish(parser.pos)?);
            }
        }
    }
}

/// Check the byte ceiling before decoding UTF-8. Invalid UTF-8 is reported at
/// the first invalid byte, including the start of an incomplete final sequence.
pub fn parse_bytes(input: &[u8], source_name: &str, limits: &ConfigLimits) -> Result<LoadedConfig> {
    check_input_bytes(input.len(), limits)?;
    parse_str(decode_utf8(input)?, source_name, limits)
}

fn error_at(code: ErrorCode, offset: usize, detail: &'static str) -> Error {
    match u32::try_from(offset) {
        Ok(offset) => config_error(code, offset, detail),
        Err(_) => config_error(ErrorCode::PayloadTooLarge, 0,
            "configuration offset exceeds u32 address space"),
    }
}

fn check_input_bytes(len: usize, limits: &ConfigLimits) -> Result<()> {
    let len = u32::try_from(len).map_err(|_| config_error(ErrorCode::PayloadTooLarge,
        0, "configuration exceeds u32 address space"))?;
    if len > limits.max_input_bytes {
        return Err(config_error(ErrorCode::PayloadTooLarge, 0,
            "configuration exceeds input byte limit"));
    }
    if limits.max_depth == 0 || limits.max_nodes == 0 {
        return Err(config_error(ErrorCode::ConstraintViolation, 0,
            "configuration limits must permit a root node"));
    }
    Ok(())
}

fn decode_utf8(input: &[u8]) -> Result<&str> {
    core::str::from_utf8(input).map_err(|error| error_at(ErrorCode::InvalidUtf8,
        error.valid_up_to(), "configuration is not valid UTF-8"))
}

fn check_string_size(len: usize, limits: &ConfigLimits, offset: usize) -> Result<()> {
    let len = u32::try_from(len).map_err(|_| error_at(ErrorCode::DecodedArenaOverflow,
        offset, "decoded string exceeds u32 address space"))?;
    if len > limits.max_string_bytes {
        return Err(error_at(ErrorCode::DecodedArenaOverflow, offset,
            "decoded string exceeds configured byte limit"));
    }
    Ok(())
}

fn reserve_string(string: &mut String, additional: usize, offset: usize) -> Result<()> {
    string.try_reserve(additional).map_err(|_| error_at(ErrorCode::BudgetExhausted,
        offset, "cannot allocate configuration string"))
}

fn make_node(start: usize, end: usize, value: ConfigValue) -> Result<ConfigNode> {
    let len = end.checked_sub(start).ok_or_else(|| error_at(ErrorCode::InternalError,
        start, "configuration span ends before it starts"))?;
    let offset = u32::try_from(start).map_err(|_| error_at(ErrorCode::PayloadTooLarge,
        0, "configuration offset exceeds u32 address space"))?;
    let len = u32::try_from(len).map_err(|_| error_at(ErrorCode::PayloadTooLarge,
        start, "configuration span exceeds u32 address space"))?;
    offset.checked_add(len).ok_or_else(|| error_at(ErrorCode::PayloadTooLarge,
        start, "configuration span exceeds u32 address space"))?;
    Ok(ConfigNode { offset, len, value })
}

struct Parser<'a> {
    input: &'a str,
    limits: &'a ConfigLimits,
    pos: usize,
    nodes: u32,
}

struct Token {
    start: usize,
    end: usize,
    kind: TokenKind,
}

enum TokenKind {
    Identifier(String),
    String(String),
    Bool(bool),
    U64(u64),
    I64(i64),
    Open(u8),
}

enum Step {
    Value(ConfigNode),
    Open(Frame),
}

impl Parser<'_> {
    fn byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn following_byte(&self) -> Option<u8> {
        self.pos.checked_add(1).and_then(|pos| self.input.as_bytes().get(pos)).copied()
    }

    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            let Some(character) = self.input[self.pos..].chars().next() else {
                return Ok(());
            };
            if character.is_whitespace() {
                self.pos += character.len_utf8();
                continue;
            }
            if self.byte() != Some(b'/') {
                return Ok(());
            }
            match self.following_byte() {
                Some(b'/') => {
                    self.pos += 2;
                    while let Some(byte) = self.byte() {
                        if byte == b'\n' || byte == b'\r' {
                            break;
                        }
                        // A line comment may contain arbitrary valid Unicode.
                        let character = self.input[self.pos..].chars().next().ok_or_else(||
                            error_at(ErrorCode::InternalError, self.pos,
                                "line comment cursor is invalid"))?;
                        self.pos += character.len_utf8();
                    }
                }
                Some(b'*') => {
                    let mut depth = 1u32;
                    self.pos += 2;
                    while depth != 0 {
                        match (self.byte(), self.following_byte()) {
                            (None, _) => return Err(error_at(ErrorCode::UnexpectedEof,
                                self.pos, "block comment is not closed")),
                            (Some(b'/'), Some(b'*')) => {
                                if depth >= u32::from(self.limits.max_depth) {
                                    return Err(error_at(ErrorCode::DepthLimitExceeded,
                                        self.pos, "block comment nesting exceeds depth limit"));
                                }
                                depth += 1;
                                self.pos += 2;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                self.pos += 2;
                            }
                            _ => {
                                let character = self.input[self.pos..].chars().next()
                                    .ok_or_else(|| error_at(ErrorCode::UnexpectedEof,
                                        self.pos, "block comment is not closed"))?;
                                self.pos += character.len_utf8();
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn can_start_node(&self, depth: u32) -> Result<()> {
        if depth > u32::from(self.limits.max_depth) {
            return Err(error_at(ErrorCode::DepthLimitExceeded, self.pos,
                "configuration nesting exceeds depth limit"));
        }
        if self.nodes >= self.limits.max_nodes {
            return Err(error_at(ErrorCode::BudgetExhausted, self.pos,
                "configuration node budget exhausted"));
        }
        Ok(())
    }

    fn value_token(&mut self, depth: u32) -> Result<Token> {
        self.skip_trivia()?;
        self.can_start_node(depth)?;
        self.token()
    }

    fn token(&mut self) -> Result<Token> {
        let start = self.pos;
        let kind = match self.byte() {
            None => return Err(error_at(ErrorCode::UnexpectedEof, start,
                "expected a configuration value")),
            Some(b'[' | b'(' | b'{') => {
                let opener = self.input.as_bytes()[self.pos];
                self.pos += 1;
                TokenKind::Open(opener)
            }
            Some(b'"') => TokenKind::String(self.string()?),
            Some(b'+' | b'-' | b'0'..=b'9') => self.number()?,
            Some(byte) if identifier_start(byte) => {
                self.pos += 1;
                while self.byte().is_some_and(identifier_continue) {
                    self.pos += 1;
                }
                let spelling = &self.input[start..self.pos];
                if ((spelling == "r" || spelling == "br")
                    && matches!(self.byte(), Some(b'#' | b'"')))
                    || (spelling == "b" && matches!(self.byte(), Some(b'"' | b'\'')))
                {
                    return Err(error_at(ErrorCode::UnsupportedSchemaFeature, start,
                        "raw strings and byte literals are unsupported"));
                }
                match spelling {
                    "true" => TokenKind::Bool(true),
                    "false" => TokenKind::Bool(false),
                    _ => {
                        check_string_size(spelling.len(), self.limits, start)?;
                        let mut name = String::new();
                        reserve_string(&mut name, spelling.len(), start)?;
                        name.push_str(spelling);
                        TokenKind::Identifier(name)
                    }
                }
            }
            Some(b'\'' | b'#') => return Err(error_at(ErrorCode::UnsupportedSchemaFeature,
                start, "character literals and extension directives are unsupported")),
            Some(b'.') if self.following_byte().is_some_and(|byte| byte.is_ascii_digit()) =>
                return Err(error_at(ErrorCode::UnsupportedSchemaFeature, start,
                    "floating point literals are unsupported")),
            Some(_) => return Err(error_at(ErrorCode::InvalidSyntax, start,
                "expected a supported configuration value")),
        };
        Ok(Token { start, end: self.pos, kind })
    }

    fn start_value(&mut self, token: Token, depth: u32) -> Result<Step> {
        self.can_start_node(depth)?;
        self.nodes = self.nodes.checked_add(1).ok_or_else(|| error_at(
            ErrorCode::BudgetExhausted, token.start, "configuration node count overflow"))?;
        let value = match token.kind {
            TokenKind::Open(opener) => return Ok(Step::Open(Frame::new(
                token.start, depth, opener, None))),
            TokenKind::Bool(value) => ConfigValue::Bool(value),
            TokenKind::Identifier(name) => {
                self.skip_trivia()?;
                if self.byte() == Some(b'(') {
                    let payload_depth = depth.checked_add(1).ok_or_else(|| error_at(
                        ErrorCode::DepthLimitExceeded, self.pos, "variant depth overflow"))?;
                    self.can_start_node(payload_depth)?;
                    self.nodes = self.nodes.checked_add(1).ok_or_else(|| error_at(
                        ErrorCode::BudgetExhausted, self.pos, "configuration node count overflow"))?;
                    let start = self.pos;
                    self.pos += 1;
                    return Ok(Step::Open(Frame::new(start, payload_depth, b'(',
                        Some((token.start, name)))));
                }
                ConfigValue::Identifier(name)
            }
            TokenKind::String(value) => ConfigValue::String(value),
            TokenKind::U64(value) => ConfigValue::U64(value),
            TokenKind::I64(value) => ConfigValue::I64(value),
        };
        Ok(Step::Value(make_node(token.start, token.end, value)?))
    }

    fn number(&mut self) -> Result<TokenKind> {
        let start = self.pos;
        let negative = self.byte() == Some(b'-');
        if matches!(self.byte(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while self.byte().is_some_and(|byte| byte.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == digits_start {
            if self.byte() == Some(b'.')
                && self.following_byte().is_some_and(|byte| byte.is_ascii_digit())
            {
                return Err(error_at(ErrorCode::UnsupportedSchemaFeature, self.pos,
                    "floating point literals are unsupported"));
            }
            return Err(error_at(ErrorCode::InvalidSyntax, self.pos,
                "integer sign must be followed by decimal digits"));
        }
        let radix_prefix = self.pos == digits_start + 1
            && self.input.as_bytes()[digits_start] == b'0'
            && matches!(self.byte(), Some(b'x' | b'X' | b'o' | b'O' | b'b' | b'B'));
        if radix_prefix || matches!(self.byte(), Some(b'.' | b'_' | b'e' | b'E')) {
            return Err(error_at(ErrorCode::UnsupportedSchemaFeature, self.pos,
                "only plain decimal integers are supported"));
        }
        if self.byte().is_some_and(identifier_start) {
            return Err(error_at(ErrorCode::InvalidSyntax, self.pos,
                "invalid character after decimal integer"));
        }
        let ceiling = if negative { (i64::MAX as u64) + 1 } else { u64::MAX };
        let mut magnitude = 0u64;
        for (index, &byte) in self.input.as_bytes()[digits_start..self.pos].iter().enumerate() {
            magnitude = magnitude.checked_mul(10)
                .and_then(|value| value.checked_add(u64::from(byte - b'0')))
                .filter(|&value| value <= ceiling)
                .ok_or_else(|| error_at(ErrorCode::NumberOverflow, digits_start + index,
                    "decimal integer exceeds its 64-bit domain"))?;
        }
        if negative {
            let value = if magnitude == (i64::MAX as u64) + 1 {
                i64::MIN
            } else {
                -i64::try_from(magnitude).map_err(|_| error_at(ErrorCode::NumberOverflow,
                    start, "negative integer exceeds i64 domain"))?
            };
            Ok(TokenKind::I64(value))
        } else {
            Ok(TokenKind::U64(magnitude))
        }
    }

    fn append_character(&self, output: &mut String, character: char, offset: usize) -> Result<()> {
        let len = output.len().checked_add(character.len_utf8()).ok_or_else(||
            error_at(ErrorCode::DecodedArenaOverflow, offset, "decoded string length overflow"))?;
        check_string_size(len, self.limits, offset)?;
        reserve_string(output, character.len_utf8(), offset)?;
        output.push(character);
        Ok(())
    }

    fn string(&mut self) -> Result<String> {
        self.pos += 1; // Opening quote has already been recognized.
        let mut output = String::new();
        loop {
            let offset = self.pos;
            let character = match self.byte() {
                None => return Err(error_at(ErrorCode::UnclosedString, self.pos,
                    "quoted string is not closed")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(output);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escaped = self.byte().ok_or_else(|| error_at(ErrorCode::UnclosedString,
                        self.pos, "quoted string ends inside an escape"))?;
                    self.pos += 1;
                    match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'0' => '\0',
                        b'u' => self.unicode_escape()?,
                        _ => return Err(error_at(ErrorCode::InvalidEscape, self.pos - 1,
                            "unsupported quoted string escape")),
                    }
                }
                Some(_) => {
                    let character = self.input[self.pos..].chars().next().ok_or_else(||
                        error_at(ErrorCode::UnclosedString, self.pos, "quoted string is not closed"))?;
                    if character.is_control() {
                        return Err(error_at(ErrorCode::ControlCharInString, self.pos,
                            "unescaped control character in quoted string"));
                    }
                    self.pos += character.len_utf8();
                    character
                }
            };
            self.append_character(&mut output, character, offset)?;
        }
    }

    fn unicode_escape(&mut self) -> Result<char> {
        if self.byte().is_none() {
            return Err(error_at(ErrorCode::UnclosedString, self.pos,
                "quoted string ends inside a Unicode escape"));
        }
        if self.byte() != Some(b'{') {
            return Err(error_at(ErrorCode::InvalidEscape, self.pos,
                "Unicode escape requires an opening brace"));
        }
        self.pos += 1;
        let digits_start = self.pos;
        let mut scalar = 0u32;
        let mut digits = 0u8;
        loop {
            match self.byte() {
                None => return Err(error_at(ErrorCode::UnclosedString, self.pos,
                    "quoted string ends inside a Unicode escape")),
                Some(b'}') => {
                    if digits == 0 {
                        return Err(error_at(ErrorCode::InvalidEscape, self.pos,
                            "Unicode escape requires one to six hex digits"));
                    }
                    self.pos += 1;
                    break;
                }
                Some(byte) => {
                    let digit = match byte {
                        b'0'..=b'9' => u32::from(byte - b'0'),
                        b'a'..=b'f' => u32::from(byte - b'a') + 10,
                        b'A'..=b'F' => u32::from(byte - b'A') + 10,
                        _ => return Err(error_at(ErrorCode::InvalidEscape, self.pos,
                            "Unicode escape contains a non-hex character")),
                    };
                    if digits == 6 {
                        return Err(error_at(ErrorCode::InvalidEscape, self.pos,
                            "Unicode escape has more than six hex digits"));
                    }
                    scalar = scalar.checked_mul(16).and_then(|value| value.checked_add(digit))
                        .ok_or_else(|| error_at(ErrorCode::InvalidEscape, self.pos,
                            "Unicode escape value overflow"))?;
                    digits += 1;
                    self.pos += 1;
                }
            }
        }
        if (0xd800..=0xdfff).contains(&scalar) {
            return Err(error_at(ErrorCode::LoneSurrogate, digits_start,
                "Unicode escape denotes a surrogate"));
        }
        char::from_u32(scalar).ok_or_else(|| error_at(ErrorCode::InvalidEscape,
            digits_start, "Unicode escape is outside the scalar range"))
    }
}

fn identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn identifier_continue(byte: u8) -> bool {
    identifier_start(byte) || byte.is_ascii_digit()
}

#[derive(Clone, Copy)]
enum CollectionKind {
    Sequence,
    Undecided,
    Tuple,
    Record,
}

struct Frame {
    start: usize,
    depth: u32,
    closer: u8,
    kind: CollectionKind,
    after_value: bool,
    values: Vec<ConfigNode>,
    fields: Vec<(String, ConfigNode)>,
    keys: BTreeSet<String>,
    pending_key: Option<String>,
    wrapper: Option<(usize, String)>,
}

enum FrameAction {
    Value(Token, u32),
    Close,
}

impl Frame {
    fn new(start: usize, depth: u32, opener: u8, wrapper: Option<(usize, String)>) -> Self {
        let (kind, closer) = match opener {
            b'[' => (CollectionKind::Sequence, b']'),
            b'{' => (CollectionKind::Record, b'}'),
            _ => (CollectionKind::Undecided, b')'),
        };
        Self { start, depth, closer, kind, after_value: false, values: Vec::new(),
            fields: Vec::new(), keys: BTreeSet::new(), pending_key: None, wrapper }
    }

    fn next(&mut self, parser: &mut Parser<'_>) -> Result<FrameAction> {
        parser.skip_trivia()?;
        if self.after_value {
            match parser.byte() {
                Some(byte) if byte == self.closer => {
                    parser.pos += 1;
                    return Ok(FrameAction::Close);
                }
                Some(b',') => {
                    parser.pos += 1;
                    parser.skip_trivia()?;
                    self.after_value = false;
                }
                None => return Err(error_at(ErrorCode::UnexpectedEof, parser.pos,
                    "collection is not closed")),
                Some(_) => return Err(error_at(ErrorCode::InvalidSyntax, parser.pos,
                    "expected a comma or collection closing delimiter")),
            }
        }
        match parser.byte() {
            Some(byte) if byte == self.closer => {
                parser.pos += 1;
                return Ok(FrameAction::Close);
            }
            None => return Err(error_at(ErrorCode::UnexpectedEof, parser.pos,
                "collection is not closed")),
            _ => {}
        }
        let count = match self.kind {
            CollectionKind::Record => self.fields.len(),
            _ => self.values.len(),
        };
        let count = u32::try_from(count).map_err(|_| error_at(ErrorCode::BudgetExhausted,
            parser.pos, "collection size exceeds u32 address space"))?;
        if count >= parser.limits.max_collection_items {
            return Err(error_at(ErrorCode::BudgetExhausted, parser.pos,
                "configuration collection item budget exhausted"));
        }
        let depth = self.depth.checked_add(1).ok_or_else(|| error_at(
            ErrorCode::DepthLimitExceeded, parser.pos, "collection depth overflow"))?;
        let token = parser.value_token(depth)?;
        parser.skip_trivia()?;
        if matches!(self.kind, CollectionKind::Undecided) {
            self.kind = if parser.byte() == Some(b':')
                && matches!(&token.kind,
                    TokenKind::Identifier(_) | TokenKind::String(_) | TokenKind::Bool(_))
            {
                CollectionKind::Record
            } else {
                CollectionKind::Tuple
            };
        }
        if matches!(self.kind, CollectionKind::Record) {
            let key = match token.kind {
                TokenKind::Identifier(key) | TokenKind::String(key) => key,
                TokenKind::Bool(value) => {
                    let spelling = if value { "true" } else { "false" };
                    check_string_size(spelling.len(), parser.limits, token.start)?;
                    let mut key = String::new();
                    reserve_string(&mut key, spelling.len(), token.start)?;
                    key.push_str(spelling);
                    key
                }
                _ => return Err(error_at(ErrorCode::InvalidSyntax, token.start,
                    "record key must be an identifier or quoted string")),
            };
            if parser.byte() != Some(b':') {
                let code = if parser.byte().is_none() {
                    ErrorCode::UnexpectedEof
                } else {
                    ErrorCode::InvalidSyntax
                };
                return Err(error_at(code, parser.pos, "expected a colon after record key"));
            }
            if self.keys.contains(&key) {
                return Err(error_at(ErrorCode::DuplicateObjectKey, token.start,
                    "record contains a duplicate key"));
            }
            // The index provides logarithmic duplicate detection without
            // changing the source ordering retained in `fields`.
            let mut indexed_key = String::new();
            reserve_string(&mut indexed_key, key.len(), token.start)?;
            indexed_key.push_str(&key);
            self.keys.insert(indexed_key);
            self.pending_key = Some(key);
            parser.pos += 1;
            return Ok(FrameAction::Value(parser.value_token(depth)?, depth));
        }
        Ok(FrameAction::Value(token, depth))
    }

    fn accept(&mut self, node: ConfigNode) -> Result<()> {
        let offset = node.offset;
        if matches!(self.kind, CollectionKind::Record) {
            let key = self.pending_key.take().ok_or_else(|| config_error(
                ErrorCode::InternalError, offset, "record has no pending key"))?;
            self.fields.try_reserve(1).map_err(|_| config_error(ErrorCode::BudgetExhausted,
                offset, "cannot allocate record entry"))?;
            self.fields.push((key, node));
        } else {
            self.values.try_reserve(1).map_err(|_| config_error(ErrorCode::BudgetExhausted,
                offset, "cannot allocate collection element"))?;
            self.values.push(node);
        }
        self.after_value = true;
        Ok(())
    }

    fn finish(self, end: usize) -> Result<ConfigNode> {
        let value = match self.kind {
            CollectionKind::Sequence => ConfigValue::Sequence(self.values),
            CollectionKind::Undecided => ConfigValue::Unit,
            CollectionKind::Tuple => ConfigValue::Tuple(self.values),
            CollectionKind::Record => ConfigValue::Record(self.fields),
        };
        let payload = make_node(self.start, end, value)?;
        match self.wrapper {
            Some((start, name)) => make_node(start, end,
                ConfigValue::Variant { name, value: Box::new(payload) }),
            None => Ok(payload),
        }
    }
}

/// Read only the supplied file. Non-UTF-8 paths receive a fixed diagnostic
/// label so producing that label never requires an unbounded lossy allocation.
#[cfg(feature = "std")]
pub fn load_file(path: &std::path::Path, limits: &ConfigLimits) -> Result<LoadedConfig> {
    validate_input_size("", limits)?;
    let label = path.to_str().unwrap_or("<non-UTF-8 path>");
    check_string_size(label.len(), limits, 0)?;
    let input = read_file_bounded(path, limits.max_input_bytes)?;
    parse_bytes(&input, label, limits)
}

/// Read bounded config and version documents, then use the root's complete
/// normalization, validation and version pipeline. No neighboring file is
/// inferred when a version path is absent.
#[cfg(feature = "std")]
pub fn load_model_files(
    config_path: &std::path::Path,
    version_path: Option<&std::path::Path>,
    options: &LoadOptions,
) -> Result<EndpointModel> {
    if options.require_version && version_path.is_none() {
        return Err(config_error(ErrorCode::MissingRequiredField, 0,
            "version.toml path is required"));
    }
    validate_input_size("", &options.limits)?;
    let config = read_file_bounded(config_path, options.limits.max_input_bytes)?;
    let version = version_path.map(|path| read_file_bounded(path, 64 * 1024)).transpose()?;
    let config = decode_utf8(&config)?;
    let version = version.as_deref().map(decode_utf8).transpose()?;
    crate::load_model(config, version, options)
}

#[cfg(feature = "std")]
fn read_file_bounded(path: &std::path::Path, ceiling: u32) -> Result<Vec<u8>> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|_| config_error(
        ErrorCode::ConstraintViolation, 0, "cannot open configuration file"))?;
    let read_limit = u64::from(ceiling).checked_add(1).ok_or_else(|| config_error(
        ErrorCode::PayloadTooLarge, 0, "configuration read limit overflow"))?;
    let mut reader = file.take(read_limit);
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => return Ok(output),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(config_error(ErrorCode::ConstraintViolation, 0,
                "cannot read configuration file")),
        };
        let len = output.len().checked_add(count).ok_or_else(|| config_error(
            ErrorCode::PayloadTooLarge, 0, "configuration read length overflow"))?;
        let len = u32::try_from(len).map_err(|_| config_error(ErrorCode::PayloadTooLarge,
            0, "configuration exceeds u32 address space"))?;
        if len > ceiling {
            return Err(config_error(ErrorCode::PayloadTooLarge, 0,
                "configuration exceeds file byte limit"));
        }
        output.try_reserve(count).map_err(|_| config_error(ErrorCode::BudgetExhausted,
            0, "cannot allocate configuration file buffer"))?;
        output.extend_from_slice(&buffer[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use crate::ErrorSource;

    fn parse(input: &str) -> LoadedConfig {
        parse_str(input, "test", &ConfigLimits::default()).unwrap()
    }

    fn failure(input: &str, code: ErrorCode, offset: u32) {
        let error = parse_str(input, "test", &ConfigLimits::default()).unwrap_err();
        assert_eq!(error.code, code, "input: {input:?}; error: {error:?}");
        assert_eq!(error.byte_offset, offset, "input: {input:?}");
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn scalar_domains_and_integer_boundaries() {
        let vectors = [
            ("()", ConfigValue::Unit),
            ("true", ConfigValue::Bool(true)),
            ("false", ConfigValue::Bool(false)),
            ("None", ConfigValue::Identifier(String::from("None"))),
            ("_Tag09", ConfigValue::Identifier(String::from("_Tag09"))),
            ("UInt32", ConfigValue::Identifier(String::from("UInt32"))),
            ("0", ConfigValue::U64(0)),
            ("+0", ConfigValue::U64(0)),
            ("00012", ConfigValue::U64(12)),
            ("-0", ConfigValue::I64(0)),
            ("+18446744073709551615", ConfigValue::U64(u64::MAX)),
            ("9223372036854775808", ConfigValue::U64(9_223_372_036_854_775_808)),
            ("-9223372036854775808", ConfigValue::I64(i64::MIN)),
            ("-9223372036854775807", ConfigValue::I64(-i64::MAX)),
        ];
        for (input, expected) in vectors {
            let loaded = parse(input);
            assert_eq!(loaded.source_name, "test");
            assert_eq!(loaded.root.value, expected, "input: {input}");
            assert_eq!(loaded.root.offset, 0);
            assert_eq!(usize::try_from(loaded.root.len).unwrap(), input.len());
        }
        failure("18446744073709551616", ErrorCode::NumberOverflow, 19);
        failure("-9223372036854775809", ErrorCode::NumberOverflow, 19);
        failure("+", ErrorCode::InvalidSyntax, 1);
        failure("--1", ErrorCode::InvalidSyntax, 1);
        failure("12name", ErrorCode::InvalidSyntax, 2);
    }

    #[test]
    fn unicode_strings_and_every_supported_escape() {
        assert_eq!(parse(r#""\"\\\n\r\t\0\u{41}\u{000061}\u{1F642}""#).root.value,
            ConfigValue::String(String::from("\"\\\n\r\t\0Aa🙂")));
        assert_eq!(parse("\"éខ🙂\"").root.value,
            ConfigValue::String(String::from("éខ🙂")));
        assert_eq!(parse(r#""\u{0}\u{7f}\u{10ffff}""#).root.value,
            ConfigValue::String(String::from("\0\u{7f}\u{10ffff}")));
        assert_eq!(parse(r#""\u{d7ff}\u{e000}""#).root.value,
            ConfigValue::String(String::from("\u{d7ff}\u{e000}")));
        assert_eq!(parse("\"\"").root.value, ConfigValue::String(String::new()));
    }

    #[test]
    fn string_failures_have_exact_byte_locations() {
        failure("\"abc", ErrorCode::UnclosedString, 4);
        failure("\"\\", ErrorCode::UnclosedString, 2);
        failure("\"\\u", ErrorCode::UnclosedString, 3);
        failure("\"\\u{", ErrorCode::UnclosedString, 4);
        failure("\"\\u{12", ErrorCode::UnclosedString, 6);
        failure(r#""\q""#, ErrorCode::InvalidEscape, 2);
        failure(r#""\u1234""#, ErrorCode::InvalidEscape, 3);
        failure(r#""\u{}""#, ErrorCode::InvalidEscape, 4);
        failure(r#""\u{xyz}""#, ErrorCode::InvalidEscape, 4);
        failure(r#""\u{1234567}""#, ErrorCode::InvalidEscape, 10);
        failure(r#""\u{d800}""#, ErrorCode::LoneSurrogate, 4);
        failure(r#""\u{DFFF}""#, ErrorCode::LoneSurrogate, 4);
        failure(r#""\u{110000}""#, ErrorCode::InvalidEscape, 4);
        failure("\"a\nb\"", ErrorCode::ControlCharInString, 2);
        failure("\"\0\"", ErrorCode::ControlCharInString, 1);
        failure("\"\u{7f}\"", ErrorCode::ControlCharInString, 1);
        failure("\"é\t\"", ErrorCode::ControlCharInString, 3);
    }

    #[test]
    fn collection_shapes_order_and_trailing_commas() {
        assert_eq!(parse("[]").root.value, ConfigValue::Sequence(vec![]));
        assert_eq!(parse("{}").root.value, ConfigValue::Record(vec![]));
        assert_eq!(parse("( /* comment */ )").root.value, ConfigValue::Unit);
        for input in ["(1)", "(1,)"] {
            let ConfigValue::Tuple(values) = parse(input).root.value else { panic!("expected tuple") };
            assert_eq!(values.len(), 1);
            assert_eq!(values[0].value, ConfigValue::U64(1));
        }
        let ConfigValue::Sequence(values) = parse("[true, (1, 2,), {},]").root.value else {
            panic!("expected sequence")
        };
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].value, ConfigValue::Bool(true));
        assert!(matches!(values[1].value, ConfigValue::Tuple(_)));
        assert!(matches!(values[2].value, ConfigValue::Record(_)));
        for input in ["(z: 1, \"a\": 2, true: 3,)", "{z: 1, \"a\": 2, true: 3,}"] {
            let ConfigValue::Record(fields) = parse(input).root.value else { panic!("expected record") };
            assert_eq!(fields.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
                vec!["z", "a", "true"]);
            assert_eq!(fields[2].1.value, ConfigValue::U64(3));
        }
        let ConfigValue::Record(fields) = parse("{\"\": false}").root.value else {
            panic!("expected record")
        };
        assert_eq!(fields[0].0, "");
    }

    #[test]
    fn variant_payloads_have_independent_spans_and_shapes() {
        let input = " \u{2003}Tag /* gap */ (\"é\",) // tail";
        let root = parse(input).root;
        let ConfigValue::Variant { name, value } = root.value else { panic!("expected variant") };
        assert_eq!(name, "Tag");
        let start = input.find("Tag").unwrap();
        let payload_start = input.find('(').unwrap();
        let end = input.find(')').unwrap() + 1;
        assert_eq!(root.offset, u32::try_from(start).unwrap());
        assert_eq!(root.len, u32::try_from(end - start).unwrap());
        assert_eq!(value.offset, u32::try_from(payload_start).unwrap());
        assert_eq!(value.len, u32::try_from(end - payload_start).unwrap());
        let ConfigValue::Tuple(items) = value.value else { panic!("expected tuple") };
        assert_eq!(items[0].len, 4);
        assert_eq!(items[0].offset, u32::try_from(input.find('"').unwrap()).unwrap());
        for (input, shape) in [("Tag()", 0), ("Tag(1)", 1), ("Tag(key: 1)", 2)] {
            let ConfigValue::Variant { value, .. } = parse(input).root.value else {
                panic!("expected variant")
            };
            assert!(match (shape, value.value) {
                (0, ConfigValue::Unit) | (1, ConfigValue::Tuple(_)) | (2, ConfigValue::Record(_)) => true,
                _ => false,
            });
        }
        let root = parse("None /* trailing comment */").root;
        assert_eq!(root.len, 4);
    }

    #[test]
    fn whitespace_and_nested_comments_are_lexical_only() {
        let input = "\u{2003}//🙂\r\n/* one /* two */ one */ [1/*x*/,//line\n2,] /*tail*/";
        let root = parse(input).root;
        assert_eq!(root.offset, u32::try_from(input.find('[').unwrap()).unwrap());
        let ConfigValue::Sequence(values) = root.value else { panic!("expected sequence") };
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].value, ConfigValue::U64(1));
        assert_eq!(parse("\"// /* */\"").root.value,
            ConfigValue::String(String::from("// /* */")));
        failure("/*", ErrorCode::UnexpectedEof, 2);
        failure("/* /* */", ErrorCode::UnexpectedEof, 8);
        failure("1 /*", ErrorCode::UnexpectedEof, 4);
        failure("// only a comment", ErrorCode::UnexpectedEof, 17);
        failure("", ErrorCode::UnexpectedEof, 0);
    }

    #[test]
    fn malformed_collections_duplicate_keys_and_trailing_content() {
        for (input, offset) in [
            ("[", 1), ("(a:", 3), ("{a", 2), ("[1,", 3), ("Tag(", 4),
        ] {
            failure(input, ErrorCode::UnexpectedEof, offset);
        }
        for (input, offset) in [
            ("[,]", 1), ("(,)", 1), ("{,}", 1), ("[1 2]", 3),
            ("[1,,]", 3), ("(a:1, 2)", 6), ("(1, a:2)", 5),
            ("{a 1}", 3), ("[1)", 2), ("(1]", 2), ("{1:2}", 1),
            ("(a:1, b)", 7), ("[a:1]", 2), ("}", 0), ("/", 0),
        ] {
            failure(input, ErrorCode::InvalidSyntax, offset);
        }
        failure("{a:1, a:2}", ErrorCode::DuplicateObjectKey, 6);
        failure(r#"(a:1, "\u{61}":2)"#, ErrorCode::DuplicateObjectKey, 6);
        failure("(\"é\":1, \"é\":2)", ErrorCode::DuplicateObjectKey, 9);
        failure("1 2", ErrorCode::TrailingContent, 2);
        failure("[] true", ErrorCode::TrailingContent, 3);
        failure("\"a\"\"b\"", ErrorCode::TrailingContent, 3);
        failure("None{}", ErrorCode::TrailingContent, 4);
    }

    #[test]
    fn recognized_unsupported_syntax_fails_closed() {
        for (input, offset) in [
            ("1.0", 1), ("1e3", 1), ("1E-3", 1), (".5", 0), ("-.5", 1),
            ("1_000", 1), ("0xff", 1), ("0b10", 1), ("+0o7", 2),
            ("r\"raw\"", 0), ("r#\"raw\"#", 0), ("b\"bytes\"", 0),
            ("br#\"raw bytes\"#", 0), ("b'a'", 0), ("'a'", 0),
            ("#![enable(implicit_some)] ()", 0),
        ] {
            failure(input, ErrorCode::UnsupportedSchemaFeature, offset);
        }
    }

    #[test]
    fn input_and_utf8_limits_are_checked_before_decoding() {
        let mut limits = ConfigLimits::default();
        limits.max_input_bytes = 2;
        assert!(parse_bytes(b"()", "", &limits).is_ok());
        assert_eq!(parse_bytes(b"[0]", "", &limits).unwrap_err().code, ErrorCode::PayloadTooLarge);
        assert_eq!(parse_bytes(&[b'"', 0xff, b'"'], "", &limits).unwrap_err().code,
            ErrorCode::PayloadTooLarge);
        limits.max_input_bytes = 20;
        for (input, offset) in [
            (&[0xff][..], 0), (&[b'"', 0xff, b'"'][..], 1),
            (&[b'"', 0xe2, 0x82][..], 1), (&[0xc0, 0xaf][..], 0),
            (&[b' ', 0xed, 0xa0, 0x80][..], 1),
        ] {
            let error = parse_bytes(input, "", &limits).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidUtf8);
            assert_eq!(error.byte_offset, offset);
            assert_eq!(error.source, ErrorSource::SchemaCompile);
        }
        assert_eq!(parse_bytes("\"é\"".as_bytes(), "", &limits).unwrap().root.len, 4);
        limits.max_depth = 0;
        assert_eq!(parse_bytes(&[0xff], "", &limits).unwrap_err().code,
            ErrorCode::ConstraintViolation);
        limits.max_depth = 1;
        limits.max_nodes = 0;
        assert_eq!(parse_str("0", "", &limits).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn decoded_string_limits_apply_to_values_identifiers_keys_and_labels() {
        let mut limits = ConfigLimits::default();
        limits.max_string_bytes = 4;
        assert!(parse_str(r#""\u{1f642}""#, "test", &limits).is_ok());
        assert!(parse_str("\"éé\"", "", &limits).is_ok());
        assert!(parse_str("Name", "", &limits).is_ok());
        for input in [r#""\u{1f642}a""#, "\"ééa\"", "Names", "{Names:0}", "{\"ééa\":0}"] {
            assert_eq!(parse_str(input, "", &limits).unwrap_err().code,
                ErrorCode::DecodedArenaOverflow, "input: {input}");
        }
        assert_eq!(parse_str("0", "12345", &limits).unwrap_err().code,
            ErrorCode::DecodedArenaOverflow);
        limits.max_string_bytes = 0;
        assert!(parse_str("\"\"", "", &limits).is_ok());
        assert!(parse_str("{\"\":0}", "", &limits).is_ok());
        assert!(parse_str("true", "", &limits).is_ok());
        assert!(parse_str("false", "", &limits).is_ok());
        assert_eq!(parse_str("{true:0}", "", &limits).unwrap_err().code,
            ErrorCode::DecodedArenaOverflow);
        assert_eq!(parse_str("\"a\"", "", &limits).unwrap_err().code,
            ErrorCode::DecodedArenaOverflow);
    }

    #[test]
    fn node_and_collection_budgets_include_payloads_but_exclude_keys() {
        let mut limits = ConfigLimits::default();
        for (input, nodes) in [("0", 1), ("()", 1), ("Tag()", 2),
            ("Tag(0)", 3), ("{a:0, b:1}", 3), ("[0, (1,)]", 4)]
        {
            limits.max_nodes = nodes;
            assert!(parse_str(input, "", &limits).is_ok(), "input: {input}");
            if nodes > 1 {
                limits.max_nodes -= 1;
                assert_eq!(parse_str(input, "", &limits).unwrap_err().code,
                    ErrorCode::BudgetExhausted, "input: {input}");
            }
        }
        limits.max_nodes = 20;
        limits.max_collection_items = 1;
        for input in ["[0,]", "(0,)", "{a:0,}", "Tag(a:0,)", "[[0],]"] {
            assert!(parse_str(input, "", &limits).is_ok(), "input: {input}");
        }
        for input in ["[0,1]", "(0,1)", "{a:0,b:1}", "Tag(a:0,b:1)"] {
            assert_eq!(parse_str(input, "", &limits).unwrap_err().code,
                ErrorCode::BudgetExhausted, "input: {input}");
        }
        limits.max_collection_items = 0;
        for input in ["[]", "{}", "()", "Tag()"] {
            assert!(parse_str(input, "", &limits).is_ok());
        }
        assert_eq!(parse_str("[0]", "", &limits).unwrap_err().code, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn budget_errors_point_to_the_value_or_item_that_exceeds_the_limit() {
        let mut limits = ConfigLimits::default();
        limits.max_nodes = 2;
        let error = parse_str("[0, 1]", "", &limits).unwrap_err();
        assert_eq!((error.code, error.byte_offset), (ErrorCode::BudgetExhausted, 4));
        limits.max_nodes = 1;
        let error = parse_str("Tag /*gap*/ ()", "", &limits).unwrap_err();
        assert_eq!((error.code, error.byte_offset), (ErrorCode::BudgetExhausted, 12));
        limits.max_nodes = 10;
        limits.max_collection_items = 1;
        let error = parse_str("{a:0, b:1}", "", &limits).unwrap_err();
        assert_eq!((error.code, error.byte_offset), (ErrorCode::BudgetExhausted, 6));
        limits.max_collection_items = 10;
        limits.max_depth = 1;
        let error = parse_str("[ /*gap*/ 0]", "", &limits).unwrap_err();
        assert_eq!((error.code, error.byte_offset), (ErrorCode::DepthLimitExceeded, 10));
        limits.max_depth = 10;
        limits.max_string_bytes = 2;
        let error = parse_str("\"éa\"", "", &limits).unwrap_err();
        assert_eq!((error.code, error.byte_offset), (ErrorCode::DecodedArenaOverflow, 3));
    }

    #[test]
    fn depth_boundaries_count_values_variant_payloads_and_comments() {
        let mut limits = ConfigLimits::default();
        for (input, depth) in [("0", 1), ("[]", 1), ("[0]", 2), ("Tag()", 2),
            ("Tag(0)", 3), ("Tag(Tag())", 4), ("{a:[0]}", 3)]
        {
            limits.max_depth = depth;
            assert!(parse_str(input, "", &limits).is_ok(), "input: {input}");
            if depth > 1 {
                limits.max_depth -= 1;
                assert_eq!(parse_str(input, "", &limits).unwrap_err().code,
                    ErrorCode::DepthLimitExceeded, "input: {input}");
            }
        }
        limits.max_depth = 1;
        assert!(parse_str("/*ok*/0", "", &limits).is_ok());
        let error = parse_str("/* /* */ */0", "", &limits).unwrap_err();
        assert_eq!((error.code, error.byte_offset), (ErrorCode::DepthLimitExceeded, 3));
        limits.max_depth = 2;
        assert!(parse_str("/* /* */ */0", "", &limits).is_ok());
        let deep = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        assert_eq!(parse_str(&deep, "", &ConfigLimits::default()).unwrap_err().code,
            ErrorCode::DepthLimitExceeded);
        let exact = format!("{}0{}", "[".repeat(127), "]".repeat(127));
        assert!(parse_str(&exact, "", &ConfigLimits::default()).is_ok());
    }

    #[test]
    fn many_record_keys_preserve_source_order() {
        let mut input = String::from("{");
        for index in (0..256).rev() {
            input.push_str(&format!("key{index}:{index},"));
        }
        input.push('}');
        let ConfigValue::Record(fields) = parse(&input).root.value else { panic!("expected record") };
        assert_eq!(fields.len(), 256);
        for (position, (key, value)) in fields.iter().enumerate() {
            let index = 255 - position;
            assert_eq!(key, &format!("key{index}"));
            assert_eq!(value.value, ConfigValue::U64(u64::try_from(index).unwrap()));
        }
    }

    #[test]
    fn parsing_is_deterministic_and_does_not_open_source_labels() {
        let input = "Config(version: \"1.2.3\", services: [], roles: [\"reader\"],)";
        let first = parse_str(input, "/path/that/is/only/a/label", &ConfigLimits::default()).unwrap();
        let second = parse_bytes(input.as_bytes(), "/path/that/is/only/a/label",
            &ConfigLimits::default()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.source_name, "/path/that/is/only/a/label");
    }

    #[cfg(feature = "std")]
    mod files {
        use super::*;
        use core::sync::atomic::{AtomicUsize, Ordering};
        use std::{io::Write, path::PathBuf};

        static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

        struct TestFile(PathBuf);

        impl TestFile {
            fn new(contents: &[u8]) -> Self {
                for _ in 0..128 {
                    let index = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
                    let path = std::env::temp_dir().join(format!(
                        "jmax-endpoint-ron-load-{}-{index}.ron", std::process::id()));
                    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                        Ok(mut file) => {
                            let fixture = Self(path);
                            file.write_all(contents).unwrap();
                            return fixture;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(error) => panic!("cannot create test fixture: {error}"),
                    }
                }
                panic!("cannot obtain a unique test fixture path");
            }
        }

        impl Drop for TestFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        #[test]
        fn file_reads_are_bounded_and_preserve_labels() {
            let exact = TestFile::new(b"[0]");
            let large = TestFile::new(b"[0] ");
            let invalid = TestFile::new(&[b'"', 0xff, b'"']);
            let mut limits = ConfigLimits::default();
            limits.max_input_bytes = 3;
            let loaded = load_file(&exact.0, &limits).unwrap();
            assert_eq!(loaded.source_name, exact.0.to_str().unwrap());
            assert_eq!(loaded.root, parse("[0]").root);
            assert_eq!(load_file(&large.0, &limits).unwrap_err().code, ErrorCode::PayloadTooLarge);
            let error = load_file(&invalid.0, &limits).unwrap_err();
            assert_eq!((error.code, error.byte_offset), (ErrorCode::InvalidUtf8, 1));
            let boundary = TestFile::new(&vec![b' '; 8193]);
            assert_eq!(read_file_bounded(&boundary.0, 8192).unwrap_err().code,
                ErrorCode::PayloadTooLarge);
            assert_eq!(read_file_bounded(&boundary.0, 8193).unwrap().len(), 8193);
            assert_eq!(read_file_bounded(&exact.0, 0).unwrap_err().code, ErrorCode::PayloadTooLarge);
            let empty = TestFile::new(b"");
            assert!(read_file_bounded(&empty.0, 0).unwrap().is_empty());
        }

        #[test]
        fn missing_paths_and_invalid_budgets_are_canonical_errors() {
            let fixture = TestFile::new(b"0");
            let missing = fixture.0.clone();
            drop(fixture);
            let error = load_file(&missing, &ConfigLimits::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
            assert_eq!(error.source, ErrorSource::SchemaCompile);
            assert_eq!(error.byte_offset, 0);
            assert_eq!(error.detail, "cannot open configuration file");
            let options = LoadOptions::default();
            assert_eq!(load_model_files(&missing, None, &options).unwrap_err().code,
                ErrorCode::MissingRequiredField);
            let mut limits = ConfigLimits::default();
            limits.max_nodes = 0;
            assert_eq!(load_file(&missing, &limits).unwrap_err().code, ErrorCode::ConstraintViolation);
        }

        #[cfg(unix)]
        #[test]
        fn read_errors_have_static_canonical_diagnostics() {
            // A directory can be opened as a File on Unix, but cannot be read
            // as configuration bytes. No directory entries are enumerated.
            let error = read_file_bounded(&std::env::temp_dir(), 1024).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
            assert_eq!(error.source, ErrorSource::SchemaCompile);
            assert_eq!(error.byte_offset, 0);
            assert_eq!(error.detail, "cannot read configuration file");
        }

        #[test]
        fn model_files_use_explicit_version_and_full_pipeline() {
            let config = TestFile::new(b"(version: \"1.2.3\")");
            let version = TestFile::new(b"format_version = 1\nmodel_version = \"1.2.3\"\nminimum_runtime = \"0.1.0\"\n");
            let mismatch = TestFile::new(b"format_version = 1\nmodel_version = \"1.2.4\"\nminimum_runtime = \"0.1.0\"\n");
            let mut options = LoadOptions::default();
            let model = load_model_files(&config.0, Some(&version.0), &options).unwrap();
            assert_eq!(model.version, crate::ModelVersion::new(1, 2, 3));
            assert!(model.services.is_empty());
            assert_eq!(load_model_files(&config.0, Some(&mismatch.0), &options).unwrap_err().code,
                ErrorCode::ConstraintViolation);
            options.require_version = false;
            assert_eq!(load_model_files(&config.0, None, &options).unwrap(), model);
            let invalid = TestFile::new(b"format_version = 1\n");
            assert_eq!(load_model_files(&config.0, Some(&invalid.0), &options).unwrap_err().code,
                ErrorCode::MissingRequiredField);
            let mut exact_version = std::fs::read(&version.0).unwrap();
            exact_version.resize(64 * 1024, b' ');
            let exact_version = TestFile::new(&exact_version);
            assert_eq!(load_model_files(&config.0, Some(&exact_version.0), &options).unwrap(), model);
            let oversized = TestFile::new(&vec![b' '; 64 * 1024 + 1]);
            assert_eq!(load_model_files(&config.0, Some(&oversized.0), &options).unwrap_err().code,
                ErrorCode::PayloadTooLarge);
            let invalid_utf8 = TestFile::new(&[0xff]);
            assert_eq!(load_model_files(&config.0, Some(&invalid_utf8.0), &options).unwrap_err().code,
                ErrorCode::InvalidUtf8);
            let invalid_model = TestFile::new(b"(version: \"1.2.3\", unknown: 0)");
            assert_eq!(load_model_files(&invalid_model.0, None, &options).unwrap_err().code,
                ErrorCode::UnknownObjectKey);
            options.limits.max_input_bytes = 1;
            assert_eq!(load_model_files(&config.0, None, &options).unwrap_err().code,
                ErrorCode::PayloadTooLarge);
        }
    }
}
