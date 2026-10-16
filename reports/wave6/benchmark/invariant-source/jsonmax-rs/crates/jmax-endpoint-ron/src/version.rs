//! Strict, allocation-free parsing and validation of version requirements.
//!
//! Only the three documented assignments are supported. Parsing borrows the
//! input, performs no filesystem access, and reports UTF-8 byte offsets. Model
//! versions and runtime versions are compared in their separate namespaces.

use crate::{
    config_error, EndpointModel, Error, ErrorCode, ModelVersion, Result,
    VersionRequirements, CONFIG_FORMAT_VERSION, RUNTIME_VERSION,
};

const MAX_VERSION_BYTES: usize = 64 * 1024;

/// Parse exactly `major.minor.patch`, with three canonical decimal u32 values.
///
/// Signs, leading zeroes, surrounding whitespace, version prefixes, prerelease
/// identifiers and build metadata are rejected. Offsets are relative to `value`.
pub fn parse_semver(value: &str) -> Result<ModelVersion> {
    u32::try_from(value.len()).map_err(|_| config_error(
        ErrorCode::PayloadTooLarge, 0, "version exceeds u32 address space",
    ))?;
    semver_at(value, 0)
}

fn semver_at(value: &str, base: usize) -> Result<ModelVersion> {
    let bytes = value.as_bytes();
    let mut pos = 0;
    let mut components = [0u32; 3];
    for (index, component) in components.iter_mut().enumerate() {
        if !bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            return Err(relative_error(ErrorCode::InvalidSyntax, base, pos,
                "version component requires an ASCII decimal digit"));
        }
        if bytes[pos] == b'0' && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit) {
            return Err(relative_error(ErrorCode::InvalidSyntax, base, pos + 1,
                "version component must not have leading zeroes"));
        }
        while let Some(&byte) = bytes.get(pos).filter(|byte| byte.is_ascii_digit()) {
            *component = component.checked_mul(10)
                .and_then(|number| number.checked_add(u32::from(byte - b'0')))
                .ok_or_else(|| relative_error(ErrorCode::NumberOverflow, base, pos,
                    "version component exceeds u32"))?;
            pos += 1;
        }
        if index < 2 {
            if bytes.get(pos) != Some(&b'.') {
                return Err(relative_error(ErrorCode::InvalidSyntax, base, pos,
                    "version requires three dot-separated components"));
            }
            pos += 1;
        }
    }
    if pos != bytes.len() {
        return Err(relative_error(ErrorCode::InvalidSyntax, base, pos,
            "content follows the third version component"));
    }
    Ok(ModelVersion::new(components[0], components[1], components[2]))
}

/// Parse the bounded version.toml grammar without allocating or applying defaults.
///
/// Every key is required exactly once. Only spaces and tabs are horizontal
/// whitespace; line endings are LF or CRLF. Comments begin with `#` outside a
/// quoted value. Version strings have no escapes and must fit on one line.
/// Parsing is independent of support checks performed by [`validate_version`].
pub fn parse_version(input: &str) -> Result<VersionRequirements> {
    validate_document_len(input.len())?;
    let mut parser = VersionParser { input, pos: 0 };
    let mut format_version = None;
    let mut model_version = None;
    let mut minimum_runtime = None;

    while parser.pos < input.len() {
        parser.horizontal_space();
        match parser.byte() {
            None => break,
            Some(b'#' | b'\n' | b'\r') => {
                parser.finish_line()?;
                continue;
            }
            Some(b'[') => return Err(error_at(ErrorCode::UnsupportedSchemaFeature,
                parser.pos, "version tables are unsupported")),
            Some(b'"' | b'\'') => return Err(error_at(ErrorCode::UnsupportedSchemaFeature,
                parser.pos, "quoted version keys are unsupported")),
            _ => {}
        }
        let key_start = parser.pos;
        while parser.byte().is_some_and(|byte|
            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            parser.pos += 1;
        }
        if parser.pos == key_start {
            return Err(error_at(ErrorCode::InvalidSyntax, key_start,
                "expected a bare version key"));
        }
        let key = &input[key_start..parser.pos];
        parser.horizontal_space();
        if parser.byte() == Some(b'.') {
            return Err(error_at(ErrorCode::UnsupportedSchemaFeature, parser.pos,
                "dotted version keys are unsupported"));
        }
        if parser.byte() != Some(b'=') {
            return Err(error_at(ErrorCode::InvalidSyntax, parser.pos,
                "expected an equals sign after version key"));
        }
        parser.pos += 1;
        parser.horizontal_space();
        // Check key identity and duplication before interpreting its value.
        let duplicate = match key {
            "format_version" => format_version.is_some(),
            "model_version" => model_version.is_some(),
            "minimum_runtime" => minimum_runtime.is_some(),
            _ => return Err(error_at(ErrorCode::UnknownObjectKey, key_start,
                "unknown version key")),
        };
        if duplicate {
            return Err(error_at(ErrorCode::DuplicateObjectKey, key_start,
                "duplicate version key"));
        }
        parser.reject_unsupported_value()?;
        match key {
            "format_version" => format_version = Some(parser.unsigned()?),
            "model_version" => model_version = Some(parser.version_string()?),
            "minimum_runtime" => minimum_runtime = Some(parser.version_string()?),
            // Unknown keys have already returned an error above.
            _ => return Err(error_at(ErrorCode::UnknownObjectKey, key_start,
                "unknown version key")),
        }
        parser.finish_line()?;
    }

    Ok(VersionRequirements {
        format_version: format_version.ok_or_else(|| error_at(
            ErrorCode::MissingRequiredField, input.len(), "format_version is required",
        ))?,
        model_version: model_version.ok_or_else(|| error_at(
            ErrorCode::MissingRequiredField, input.len(), "model_version is required",
        ))?,
        minimum_runtime: minimum_runtime.ok_or_else(|| error_at(
            ErrorCode::MissingRequiredField, input.len(), "minimum_runtime is required",
        ))?,
    })
}

/// Require a supported format, an exact model version and a compatible runtime.
///
/// Checks are deterministic in that order, leave both inputs unchanged, and use
/// offset zero because the normalized model has no source-location metadata.
pub fn validate_version(model: &EndpointModel, requirements: &VersionRequirements) -> Result<()> {
    if requirements.format_version != CONFIG_FORMAT_VERSION {
        return Err(config_error(ErrorCode::UnsupportedSchemaFeature, 0,
            "unsupported configuration format version"));
    }
    if model.version != requirements.model_version {
        return Err(config_error(ErrorCode::ConstraintViolation, 0,
            "model version differs from version.toml"));
    }
    let minimum = requirements.minimum_runtime;
    if (minimum.major, minimum.minor, minimum.patch)
        > (RUNTIME_VERSION.major, RUNTIME_VERSION.minor, RUNTIME_VERSION.patch)
    {
        return Err(config_error(ErrorCode::UnsupportedSchemaFeature, 0,
            "minimum runtime exceeds the available runtime version"));
    }
    Ok(())
}

fn validate_document_len(len: usize) -> Result<()> {
    u32::try_from(len).map_err(|_| config_error(ErrorCode::PayloadTooLarge, 0,
        "version document exceeds u32 address space"))?;
    if len > MAX_VERSION_BYTES {
        return Err(config_error(ErrorCode::PayloadTooLarge, 0,
            "version document exceeds 64 KiB"));
    }
    Ok(())
}

fn error_at(code: ErrorCode, offset: usize, detail: &'static str) -> Error {
    match u32::try_from(offset) {
        Ok(offset) => config_error(code, offset, detail),
        Err(_) => config_error(ErrorCode::PayloadTooLarge, 0,
            "version offset exceeds u32 address space"),
    }
}

fn relative_error(code: ErrorCode, base: usize, offset: usize, detail: &'static str) -> Error {
    match base.checked_add(offset) {
        Some(absolute) => error_at(code, absolute, detail),
        None => config_error(ErrorCode::PayloadTooLarge, 0,
            "version offset arithmetic overflow"),
    }
}

struct VersionParser<'a> {
    input: &'a str,
    pos: usize,
}

impl VersionParser<'_> {
    fn byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn horizontal_space(&mut self) {
        while matches!(self.byte(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    fn reject_unsupported_value(&self) -> Result<()> {
        if matches!(self.byte(), Some(b'\n' | b'\r')) {
            return Err(error_at(ErrorCode::UnsupportedSchemaFeature, self.pos,
                "version values must begin on their assignment line"));
        }
        if matches!(self.byte(), Some(b'[' | b'{' | b'\''))
            || self.input.as_bytes()[self.pos..].starts_with(b"\"\"\"")
        {
            return Err(error_at(ErrorCode::UnsupportedSchemaFeature, self.pos,
                "arrays, tables and alternative version strings are unsupported"));
        }
        Ok(())
    }

    fn unsigned(&mut self) -> Result<u32> {
        if !self.byte().is_some_and(|byte| byte.is_ascii_digit()) {
            return Err(error_at(ErrorCode::InvalidSyntax, self.pos,
                "format_version requires an unsigned decimal integer"));
        }
        if self.byte() == Some(b'0')
            && self.input.as_bytes().get(self.pos + 1).is_some_and(u8::is_ascii_digit)
        {
            return Err(error_at(ErrorCode::InvalidSyntax, self.pos + 1,
                "format_version must not have leading zeroes"));
        }
        let mut value = 0u32;
        while let Some(byte) = self.byte().filter(|byte| byte.is_ascii_digit()) {
            value = value.checked_mul(10)
                .and_then(|value| value.checked_add(u32::from(byte - b'0')))
                .ok_or_else(|| error_at(ErrorCode::NumberOverflow, self.pos,
                    "format_version exceeds u32"))?;
            self.pos += 1;
        }
        Ok(value)
    }

    fn version_string(&mut self) -> Result<ModelVersion> {
        if self.byte() != Some(b'"') {
            return Err(error_at(ErrorCode::InvalidSyntax, self.pos,
                "version requires a plain double-quoted string"));
        }
        self.pos += 1;
        let start = self.pos;
        loop {
            match self.byte() {
                Some(b'"') => {
                    let version = semver_at(&self.input[start..self.pos], start)?;
                    self.pos += 1;
                    return Ok(version);
                }
                Some(b'\n' | b'\r') => return Err(error_at(
                    ErrorCode::UnsupportedSchemaFeature, self.pos,
                    "multiline version strings are unsupported",
                )),
                Some(b'\\') => return Err(error_at(ErrorCode::InvalidSyntax, self.pos,
                    "escapes in version strings are unsupported")),
                None => return Err(error_at(ErrorCode::InvalidSyntax, self.pos,
                    "version string is not closed")),
                // ASCII quote boundaries keep the eventual borrowed slice on
                // UTF-8 boundaries even while scanning non-ASCII bytes.
                Some(_) => self.pos += 1,
            }
        }
    }

    fn finish_line(&mut self) -> Result<()> {
        self.horizontal_space();
        if self.byte() == Some(b'#') {
            while !matches!(self.byte(), None | Some(b'\n' | b'\r')) {
                self.pos += 1;
            }
        }
        match self.byte() {
            None => Ok(()),
            Some(b'\n') => {
                self.pos += 1;
                Ok(())
            }
            Some(b'\r') => {
                if self.input.as_bytes().get(self.pos + 1) != Some(&b'\n') {
                    return Err(error_at(ErrorCode::InvalidSyntax, self.pos,
                        "a carriage return must be followed by a line feed"));
                }
                self.pos += 2;
                Ok(())
            }
            _ => Err(error_at(ErrorCode::InvalidSyntax, self.pos,
                "expected a comment or end of version assignment line")),
        }
    }
}
