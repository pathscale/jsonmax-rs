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
