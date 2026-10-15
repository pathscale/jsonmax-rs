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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::String};
    use crate::{ErrorSource, LoadOptions};

    const DOCUMENT: &str = "format_version = 1\nmodel_version = \"1.2.3\"\nminimum_runtime = \"0.1.0\"\n";

    fn requirements() -> VersionRequirements {
        VersionRequirements {
            format_version: CONFIG_FORMAT_VERSION,
            model_version: ModelVersion::new(1, 2, 3),
            minimum_runtime: RUNTIME_VERSION,
        }
    }

    fn model() -> EndpointModel {
        EndpointModel { version: ModelVersion::new(1, 2, 3), ..EndpointModel::default() }
    }

    fn check_error(error: Error, code: ErrorCode, offset: usize) -> Error {
        assert_eq!(error.code, code, "{error:?}");
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, u32::try_from(offset).unwrap());
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
        error
    }

    fn version_failure(input: &str, code: ErrorCode, offset: usize) -> Error {
        check_error(parse_version(input).unwrap_err(), code, offset)
    }

    #[test]
    fn semver_decimal_boundary_product_round_trips() {
        let values = [0, 1, 9, 10, 99, u32::MAX - 1, u32::MAX];
        for major in values {
            for minor in values {
                for patch in values {
                    let text = format!("{major}.{minor}.{patch}");
                    assert_eq!(parse_semver(&text), Ok(ModelVersion::new(major, minor, patch)));
                }
            }
        }
    }

    #[test]
    fn semver_syntax_vectors_have_exact_offsets() {
        for (input, offset) in [
            ("", 0), ("1", 1), ("1.2", 3), ("1.2.", 4), (".2.3", 0),
            ("1..3", 2), ("1.2.3.4", 5), ("01.2.3", 1), ("1.02.3", 3),
            ("1.2.03", 5), ("00.0.0", 1), ("+1.2.3", 0), ("-1.2.3", 0),
            ("v1.2.3", 0), (" 1.2.3", 0), ("1.2.3 ", 5), ("1.2.3-alpha", 5),
            ("1.2.3+build", 5), ("1.2.3\n", 5), ("1.2.3\0", 5),
            ("1_0.2.3", 1), ("0x1.2.3", 1), ("1e2.2.3", 1),
            ("1.２.3", 2), ("١.2.3", 0), ("1.2.雪", 4), ("1．2.3", 1),
        ] {
            check_error(parse_semver(input).unwrap_err(), ErrorCode::InvalidSyntax, offset);
        }
    }

    #[test]
    fn semver_overflow_is_reported_at_the_first_overflowing_digit() {
        for (input, offset) in [
            ("4294967296.0.0", 9), ("0.4294967296.0", 11),
            ("0.0.4294967296", 13), ("999999999999999999999.0.0", 9),
            ("42949672950.0.0", 10),
        ] {
            check_error(parse_semver(input).unwrap_err(), ErrorCode::NumberOverflow, offset);
        }
    }

    #[test]
    fn semver_single_byte_mutations_cover_the_entire_ascii_domain() {
        for index in 0..5 {
            for byte in 0u8..=127 {
                let mut bytes = *b"1.2.3";
                bytes[index] = byte;
                let input = core::str::from_utf8(&bytes).unwrap();
                let valid = if index % 2 == 0 { byte.is_ascii_digit() } else { byte == b'.' };
                assert_eq!(parse_semver(input).is_ok(), valid, "{bytes:?}");
                if !valid {
                    // Replacing a dot with a digit joins components, so the
                    // missing separator is detected at EOF rather than here.
                    let offset = if index % 2 == 1 && byte.is_ascii_digit() {
                        input.len()
                    } else {
                        index
                    };
                    check_error(parse_semver(input).unwrap_err(), ErrorCode::InvalidSyntax, offset);
                }
            }
        }
    }

    #[test]
    fn canonical_document_and_all_key_permutations_are_equivalent() {
        assert_eq!(parse_version(DOCUMENT), Ok(requirements()));
        let lines = ["format_version=1", "model_version=\"1.2.3\"", "minimum_runtime=\"0.1.0\""];
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            for ending in ["\n", "\r\n"] {
                for last_newline in [false, true] {
                    let input = format!("{}{ending}{}{ending}{}{}", lines[order[0]],
                        lines[order[1]], lines[order[2]], if last_newline { ending } else { "" });
                    assert_eq!(parse_version(&input), Ok(requirements()));
                }
            }
        }
    }

    #[test]
    fn whitespace_comments_and_unicode_comment_text_are_supported() {
        let input = " \t# 雪 = [ignored] \"quoted\"\r\n\t\r\n\
            \tformat_version\t =\t1# format\n\
            model_version = \"1.2.3\" \t # version\r\n\
            minimum_runtime=\"0.1.0\"#runtime\n\t # final λ";
        assert_eq!(parse_version(input), Ok(requirements()));
        assert_eq!(parse_version(&format!("{DOCUMENT} \t")), Ok(requirements()));
    }

    #[test]
    fn every_subset_of_required_keys_is_checked_without_defaults() {
        let lines = ["format_version=1\n", "model_version=\"1.2.3\"\n", "minimum_runtime=\"0.1.0\"\n"];
        for mask in 0..8 {
            let mut input = String::new();
            for (index, line) in lines.iter().enumerate() {
                if mask & (1 << index) != 0 { input.push_str(line); }
            }
            if mask == 7 {
                assert_eq!(parse_version(&input), Ok(requirements()));
            } else {
                let error = version_failure(&input, ErrorCode::MissingRequiredField, input.len());
                let expected = if mask & 1 == 0 { "format_version is required" }
                    else if mask & 2 == 0 { "model_version is required" }
                    else { "minimum_runtime is required" };
                assert_eq!(error.detail, expected);
            }
        }
        let input = " # only a comment 雪\r\n\t";
        version_failure(input, ErrorCode::MissingRequiredField, input.len());
    }

    #[test]
    fn duplicates_are_rejected_at_the_repeated_key_before_its_value() {
        for line in ["format_version=2", "model_version=\"9.8.7\"", "minimum_runtime=\"0.0.0\""] {
            let input = format!("{DOCUMENT}\t{line}");
            version_failure(&input, ErrorCode::DuplicateObjectKey, DOCUMENT.len() + 1);
        }
        let prefix = "format_version=1\n# λ\n";
        let input = format!("{prefix}format_version=not_a_number");
        version_failure(&input, ErrorCode::DuplicateObjectKey, prefix.len());
    }

    #[test]
    fn unknown_keys_are_case_sensitive_and_never_ignored() {
        for key in ["version", "Format_version", "modelVersion", "runtime", "extra", "0", "a-b"] {
            let input = format!("{DOCUMENT}  {key}=1");
            version_failure(&input, ErrorCode::UnknownObjectKey, DOCUMENT.len() + 2);
        }
        version_failure("unknown=[]", ErrorCode::UnknownObjectKey, 0);
    }

    #[test]
    fn unsupported_structural_and_string_forms_have_precise_offsets() {
        for (input, offset) in [
            ("[version]", 0), ("\t[[version]]", 1),
            ("format_version.value=1", 14), ("model_version . major=1", 14),
            ("\"format_version\"=1", 0), ("'format_version'=1", 0),
            ("format_version=[]", 15), ("model_version=[\"1.2.3\"]", 14),
            ("minimum_runtime={major=0}", 16), ("model_version='1.2.3'", 14),
            ("model_version=\"\"\"1.2.3\"\"\"", 14),
            ("minimum_runtime='''0.1.0'''", 16),
            ("model_version=\"1.\n2.3\"", 17),
            ("model_version=\"1.\r\n2.3\"", 17),
            ("model_version=\n\"1.2.3\"", 14),
            ("minimum_runtime=\r\n\"0.1.0\"", 16),
            ("format_version=\n1", 15),
        ] {
            version_failure(input, ErrorCode::UnsupportedSchemaFeature, offset);
        }
        let input = format!("{DOCUMENT}[extra]");
        version_failure(&input, ErrorCode::UnsupportedSchemaFeature, DOCUMENT.len());
    }

    #[test]
    fn malformed_assignment_vectors_reject_extensions_and_trailing_tokens() {
        for (input, offset) in [
            ("=1", 0), (":", 0), ("format_version", 14), ("format_version 1", 15),
            ("format_version:1", 14), ("format_version==1", 15),
            ("format_version=", 15),
            ("format_version=+1", 15), ("format_version=-1", 15),
            ("format_version=01", 16), ("format_version=1_0", 16),
            ("format_version=0x1", 16), ("format_version=1.0", 16),
            ("format_version=1e0", 16), ("format_version=true", 15),
            ("format_version=\"1\"", 15), ("format_version=１", 15),
            ("format_version=1;", 16), ("format_version=1,", 16),
            ("format_version=1 model_version=\"1.2.3\"", 17),
            ("model_version=1.2.3", 14), ("model_version=", 14),
            ("model_version=\"1.2.3", 20),
            ("model_version=\"1.2.3\"\"extra\"", 21),
            ("model_version=\"1.2.3\";", 21),
        ] {
            version_failure(input, ErrorCode::InvalidSyntax, offset);
        }
    }

    #[test]
    fn format_u32_boundaries_parse_before_compatibility_is_checked() {
        for value in [0, 1, 2, u32::MAX - 1, u32::MAX] {
            let input = format!("format_version={value}\nmodel_version=\"1.2.3\"\nminimum_runtime=\"0.1.0\"");
            let parsed = parse_version(&input).unwrap();
            assert_eq!(parsed.format_version, value);
            assert_eq!(parsed.model_version, ModelVersion::new(1, 2, 3));
            if value == CONFIG_FORMAT_VERSION {
                assert_eq!(validate_version(&model(), &parsed), Ok(()));
            } else {
                check_error(validate_version(&model(), &parsed).unwrap_err(),
                    ErrorCode::UnsupportedSchemaFeature, 0);
            }
        }
        version_failure("format_version=4294967296", ErrorCode::NumberOverflow, 24);
        version_failure("format_version=999999999999999999999", ErrorCode::NumberOverflow, 24);
    }

    #[test]
    fn version_document_preserves_full_width_model_and_runtime_components() {
        for model_version in [
            ModelVersion::new(0, 0, 0), ModelVersion::new(u32::MAX, 0, u32::MAX),
            ModelVersion::new(u32::MAX, u32::MAX, u32::MAX),
        ] {
            for minimum_runtime in [
                ModelVersion::new(0, 0, 0), ModelVersion::new(0, 0, u32::MAX),
                RUNTIME_VERSION, ModelVersion::new(u32::MAX, u32::MAX, u32::MAX),
            ] {
                let input = format!("format_version=1\nmodel_version=\"{model_version}\"\nminimum_runtime=\"{minimum_runtime}\"");
                let parsed = parse_version(&input).unwrap();
                assert_eq!(parsed, VersionRequirements {
                    format_version: CONFIG_FORMAT_VERSION, model_version, minimum_runtime,
                });
                let model = EndpointModel { version: model_version, ..EndpointModel::default() };
                assert_eq!(validate_version(&model, &parsed).is_ok(), minimum_runtime <= RUNTIME_VERSION);
            }
        }
    }

    #[test]
    fn quoted_semver_errors_are_rebased_to_absolute_utf8_byte_offsets() {
        for key in ["model_version", "minimum_runtime"] {
            for (value, code, relative) in [
                ("", ErrorCode::InvalidSyntax, 0),
                ("1.02.3", ErrorCode::InvalidSyntax, 3),
                ("1.2.3#inside", ErrorCode::InvalidSyntax, 5),
                ("1.2.3+meta", ErrorCode::InvalidSyntax, 5),
                ("1.２.3", ErrorCode::InvalidSyntax, 2),
                ("0.0.4294967296", ErrorCode::NumberOverflow, 13),
            ] {
                let prefix = format!("# 雪 λ\r\n\t{key} = \"");
                let input = format!("{prefix}{value}\"");
                version_failure(&input, code, prefix.len() + relative);
            }
        }
    }

    #[test]
    fn string_escapes_controls_and_embedded_comments_are_not_interpreted() {
        for value in [r"1.2.\u0033", r"1.2.3\n", r"1.2.3\", r"1.2.\0", r"1.2.\t"] {
            let input = format!("model_version=\"{value}\"");
            version_failure(&input, ErrorCode::InvalidSyntax, input.find('\\').unwrap());
        }
        for value in ["1.2.3\0", "1.2.3\t", "1.2.3\u{7f}", "1.2.#3", "1.2.3 # not a comment"] {
            let input = format!("model_version=\"{value}\"");
            let offset = if value == "1.2.#3" { 19 } else { 20 };
            version_failure(&input, ErrorCode::InvalidSyntax, offset);
        }
        assert_eq!(parse_version(&DOCUMENT.replace("1.2.3\"", "1.2.3\"#comment")), Ok(requirements()));
    }

    #[test]
    fn line_ending_and_horizontal_whitespace_boundaries_are_strict() {
        for input in ["\r", "\rX", "# comment\r", "format_version=1\r", "format_version=1\rmodel_version=\"1.2.3\""] {
            version_failure(input, ErrorCode::InvalidSyntax, input.find('\r').unwrap());
        }
        for whitespace in ['\u{b}', '\u{c}', '\u{a0}', '\u{2003}', '\u{feff}'] {
            let input = format!("{whitespace}{DOCUMENT}");
            version_failure(&input, ErrorCode::InvalidSyntax, 0);
            let input = format!("format_version={whitespace}1");
            version_failure(&input, ErrorCode::InvalidSyntax, 15);
        }
        let input = format!("{DOCUMENT}\0");
        version_failure(&input, ErrorCode::InvalidSyntax, DOCUMENT.len());
    }

    #[test]
    fn document_size_accepts_the_ceiling_and_rejects_the_next_byte_first() {
        let mut exact = String::from(DOCUMENT);
        exact.push('#');
        exact.push_str(&"x".repeat(MAX_VERSION_BYTES - exact.len()));
        assert_eq!(exact.len(), MAX_VERSION_BYTES);
        assert_eq!(parse_version(&exact), Ok(requirements()));
        exact.push('x');
        version_failure(&exact, ErrorCode::PayloadTooLarge, 0);
        let invalid = "!".repeat(MAX_VERSION_BYTES + 1);
        version_failure(&invalid, ErrorCode::PayloadTooLarge, 0);
        // The ceiling counts encoded bytes, including multibyte comment text.
        let mut unicode = String::from(DOCUMENT);
        unicode.push('#');
        unicode.push_str(&" ".repeat(MAX_VERSION_BYTES - unicode.len() - 2));
        unicode.push('λ');
        assert_eq!(unicode.len(), MAX_VERSION_BYTES);
        assert_eq!(parse_version(&unicode), Ok(requirements()));
        unicode.push('λ');
        version_failure(&unicode, ErrorCode::PayloadTooLarge, 0);
    }

    #[test]
    fn length_and_diagnostic_arithmetic_are_checked_without_large_allocations() {
        assert_eq!(validate_document_len(0), Ok(()));
        assert_eq!(validate_document_len(MAX_VERSION_BYTES), Ok(()));
        check_error(validate_document_len(MAX_VERSION_BYTES + 1).unwrap_err(), ErrorCode::PayloadTooLarge, 0);
        check_error(validate_document_len(usize::MAX).unwrap_err(), ErrorCode::PayloadTooLarge, 0);
        check_error(relative_error(ErrorCode::InvalidSyntax, usize::MAX, 1, "overflow"), ErrorCode::PayloadTooLarge, 0);
        if let Ok(maximum) = usize::try_from(u32::MAX) {
            let error = error_at(ErrorCode::InvalidSyntax, maximum, "maximum offset");
            assert_eq!(error.byte_offset, u32::MAX);
            if let Some(beyond) = maximum.checked_add(1) {
                check_error(error_at(ErrorCode::InvalidSyntax, beyond, "too large"), ErrorCode::PayloadTooLarge, 0);
                check_error(validate_document_len(beyond).unwrap_err(), ErrorCode::PayloadTooLarge, 0);
            }
        }
    }

    #[test]
    fn truncation_at_every_byte_never_accepts_a_partial_assignment() {
        let minimal = DOCUMENT.trim_end_matches('\n');
        for end in 0..minimal.len() {
            let result = parse_version(&minimal[..end]);
            assert!(result.is_err(), "accepted truncated document at {end}");
            let error = result.unwrap_err();
            assert_eq!(error.source, ErrorSource::SchemaCompile);
            assert!(usize::try_from(error.byte_offset).unwrap() <= end);
        }
        assert_eq!(parse_version(minimal), Ok(requirements()));
    }

    #[test]
    fn runtime_compatibility_is_lexicographic_and_inclusive() {
        for minimum in [
            ModelVersion::new(0, 0, 0), ModelVersion::new(0, 0, u32::MAX), RUNTIME_VERSION,
        ] {
            let requirements = VersionRequirements { minimum_runtime: minimum, ..requirements() };
            assert_eq!(validate_version(&model(), &requirements), Ok(()));
        }
        for minimum in [
            ModelVersion::new(0, 1, 1), ModelVersion::new(0, 2, 0),
            ModelVersion::new(1, 0, 0), ModelVersion::new(u32::MAX, 0, 0),
            ModelVersion::new(u32::MAX, u32::MAX, u32::MAX),
        ] {
            let requirements = VersionRequirements { minimum_runtime: minimum, ..requirements() };
            check_error(validate_version(&model(), &requirements).unwrap_err(), ErrorCode::UnsupportedSchemaFeature, 0);
        }
    }

    #[test]
    fn model_version_requires_equality_in_every_component() {
        for version in [
            ModelVersion::new(0, 2, 3), ModelVersion::new(2, 2, 3),
            ModelVersion::new(1, 1, 3), ModelVersion::new(1, 3, 3),
            ModelVersion::new(1, 2, 2), ModelVersion::new(1, 2, 4),
        ] {
            let model = EndpointModel { version, ..EndpointModel::default() };
            check_error(validate_version(&model, &requirements()).unwrap_err(), ErrorCode::ConstraintViolation, 0);
        }
        for version in [ModelVersion::new(0, 0, 0), ModelVersion::new(u32::MAX, u32::MAX, u32::MAX)] {
            let model = EndpointModel { version, ..EndpointModel::default() };
            let requirements = VersionRequirements { model_version: version, ..requirements() };
            assert_eq!(validate_version(&model, &requirements), Ok(()));
        }
    }

    #[test]
    fn compatibility_failure_precedence_and_immutability_are_deterministic() {
        let model = model();
        let mut requirements = VersionRequirements {
            format_version: 0,
            model_version: ModelVersion::new(9, 9, 9),
            minimum_runtime: ModelVersion::new(9, 9, 9),
        };
        let original_model = model.clone();
        let original_requirements = requirements;
        let first = validate_version(&model, &requirements).unwrap_err();
        assert_eq!(first.detail, "unsupported configuration format version");
        assert_eq!(validate_version(&model, &requirements), Err(first));
        assert_eq!(requirements, original_requirements);
        requirements.format_version = CONFIG_FORMAT_VERSION;
        let mismatch = validate_version(&model, &requirements).unwrap_err();
        assert_eq!(mismatch.code, ErrorCode::ConstraintViolation);
        requirements.model_version = model.version;
        let future = validate_version(&model, &requirements).unwrap_err();
        assert_eq!(future.detail, "minimum runtime exceeds the available runtime version");
        assert_eq!(model, original_model);
    }

    #[test]
    fn full_pipeline_parses_normalizes_validates_and_preserves_version_namespaces() {
        let source = r#"Config(
            version: "1.2.3",
            defaults: Defaults(service: "Api", roles: ["Reader"], description: "kept"),
            services: [Service(id: 0, endpoints: [Endpoint(
                code: 0, method: "read",
                response: [Field(name: "node", ty: StructRef("Node"))],
                stream_response: Some(StructTable(struct_ref: "Node")),
            )])],
            definitions: [Definition(name: "Node", ty: Struct(name: "Node", fields: [
                Field(name: "next", ty: Optional(StructRef("Node"))),
            ]))],
            roles: ["Reader"],
            errors: [Error(code: 0, name: "Failure", message: "failed")],
            headers: [Header(name: "X-Id", ty: NanoId(len: 21))],
        )"#;
        let options = LoadOptions::default();
        let parsed = crate::load::parse_str(source, "<version-test>", &options.limits).unwrap();
        let expected = crate::normalize::normalize(&parsed).unwrap();
        assert_eq!(crate::validate::validate_model(&expected), Ok(()));
        let loaded = crate::load_model(source, Some(DOCUMENT), &options).unwrap();
        assert_eq!(loaded, expected);
        assert_eq!(loaded.version, ModelVersion::new(1, 2, 3));
        assert_eq!(loaded.services[0].endpoints[0].description, "kept");
        assert_eq!(loaded.services[0].endpoints[0].service, "Api");
        assert_eq!(loaded.services[0].endpoints[0].roles[0], "Reader");
        assert_eq!(loaded.definitions.len(), 1);
    }

    #[test]
    fn full_pipeline_requires_metadata_unless_explicitly_optional() {
        let source = "(version: \"1.2.3\")";
        let mut options = LoadOptions::default();
        check_error(crate::load_model(source, None, &options).unwrap_err(), ErrorCode::MissingRequiredField, 0);
        options.require_version = false;
        assert_eq!(crate::load_model(source, None, &options), Ok(model()));
        assert_eq!(crate::load_model(source, Some(DOCUMENT), &options), Ok(model()));
        check_error(crate::load_model(source, Some(""), &options).unwrap_err(), ErrorCode::MissingRequiredField, 0);
        let mismatch = DOCUMENT.replace("1.2.3", "1.2.4");
        check_error(crate::load_model(source, Some(&mismatch), &options).unwrap_err(), ErrorCode::ConstraintViolation, 0);
        let future = DOCUMENT.replace("0.1.0", "0.1.1");
        check_error(crate::load_model(source, Some(&future), &options).unwrap_err(), ErrorCode::UnsupportedSchemaFeature, 0);
    }

    #[test]
    fn full_pipeline_never_overwrites_explicit_config_versions() {
        let options = LoadOptions::default();
        for source in [
            "(version: \"1.2.4\")",
            "(version: Version(major: 1, minor: 2, patch: 4))",
        ] {
            check_error(crate::load_model(source, Some(DOCUMENT), &options).unwrap_err(), ErrorCode::ConstraintViolation, 0);
        }
        let source = "(version: Version(major: 1, minor: 2, patch: 3))";
        assert_eq!(crate::load_model(source, Some(DOCUMENT), &options), Ok(model()));
    }

    #[test]
    fn full_pipeline_error_order_is_parse_then_normalize_then_semantics_then_compatibility() {
        let options = LoadOptions::default();
        let malformed_metadata = "format_version=oops";
        check_error(crate::load_model("!", Some(malformed_metadata), &options).unwrap_err(), ErrorCode::InvalidSyntax, 15);
        let unsupported_format = DOCUMENT.replace("format_version = 1", "format_version = 2");
        check_error(crate::load_model("!", Some(&unsupported_format), &options).unwrap_err(), ErrorCode::InvalidSyntax, 0);
        let missing_config_version = "(services: [])";
        check_error(crate::load_model(missing_config_version, Some(&unsupported_format), &options).unwrap_err(), ErrorCode::MissingRequiredField, 0);
        let invalid_model = r#"(version: "1.2.3", definitions: [(name: "Unused", ty: StructRef("Missing"))])"#;
        let error = crate::load_model(invalid_model, Some(&unsupported_format), &options).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.detail, "unknown type reference");
        check_error(crate::load_model("(version: \"1.2.3\")", Some(&unsupported_format), &options).unwrap_err(), ErrorCode::UnsupportedSchemaFeature, 0);
    }
}
