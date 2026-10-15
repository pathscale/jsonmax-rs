//! Versioned legacy schema artifacts and byte-preserving documentation adapters.
//!
//! The application supplies the legacy grammar through [`LegacySchemaCodec`].
//! Its decoder must consume the entire input and enforce UTF-8, syntax, depth,
//! and decoded-memory limits without dropping unsupported constraints. This
//! module validates the returned IR independently; it never issues a JSON proof.

use alloc::{string::String, vec::Vec};
use jmax_abi::schema::SchemaFieldIr;

use super::{
    BoundedSink, ByteSink, CompatOptions, CompatibilityWarning, Conversion, Error,
    ErrorCode, ErrorSource, LegacyDocumentation, LegacySchema, LegacySchemaCodec,
    LossPolicy, Payload, Result, SchemaIr, LEGACY_FORMAT_VERSION,
};

/// Decode an application-defined legacy artifact and validate its native IR.
///
/// Field order, identifiers, scalar kinds, formats, and numeric constraints are
/// transferred exactly. A nonempty legacy description is the only documented
/// loss: rejecting policy fails, while allowing policy returns an explicit warning.
pub fn import_schema(
    input: &[u8],
    codec: &mut dyn LegacySchemaCodec,
    options: CompatOptions,
) -> Result<Conversion<SchemaIr>> {
    schema_version(LEGACY_FORMAT_VERSION, options)?;
    if input.len() > options.limits.max_payload_bytes as usize {
        return Err(schema_error(
            ErrorCode::PayloadTooLarge,
            "legacy schema input byte limit exceeded",
        ));
    }
    let legacy = codec.decode_schema(input, options.limits).map_err(schema_source)?;
    schema_version(legacy.version, options)?;
    validate_schema(&legacy.schema)?;

    if legacy.description.is_empty() {
        return Ok(Conversion::exact(legacy.schema));
    }
    if options.loss_policy == LossPolicy::Reject {
        return Err(schema_error(
            ErrorCode::UnsupportedSchemaFeature,
            "legacy schema description is not represented by SchemaIr",
        ).with_schema(legacy.schema.schema_id.0, 0));
    }

    let warning = CompatibilityWarning {
        path: copy_string("description", ErrorSource::SchemaCompile)?,
        detail: copy_string(
            "legacy schema description is not represented by SchemaIr",
            ErrorSource::SchemaCompile,
        )?,
    };
    let mut warnings = Vec::new();
    reserve(&mut warnings, 1)?;
    warnings.push(warning);
    Ok(Conversion { value: legacy.schema, warnings })
}

/// Encode validated IR using the application's declared legacy artifact format.
///
/// The supplied description is represented in the legacy wrapper and is not a
/// lossy conversion. Only `max_output_bytes` bounds this opaque artifact; JSON
/// payload limits and JSON validation do not apply to the encoder's output.
pub fn export_schema(
    schema: &SchemaIr,
    description: &str,
    codec: &mut dyn LegacySchemaCodec,
    options: CompatOptions,
) -> Result<Payload> {
    schema_version(LEGACY_FORMAT_VERSION, options)?;
    validate_schema(schema)?;
    let legacy = LegacySchema {
        version: LEGACY_FORMAT_VERSION,
        schema: copy_schema(schema)?,
        description: copy_string(description, ErrorSource::SchemaCompile)?,
    };
    let mut sink = SchemaSink {
        inner: BoundedSink::new(options.max_output_bytes),
        failure: None,
    };
    let encoded = codec.encode_schema(&legacy, &mut sink);
    // A codec cannot publish a prefix after ignoring or replacing a sink error.
    if let Some(error) = sink.failure {
        return Err(error);
    }
    encoded.map_err(schema_source)?;
    Ok(Payload::from_vec(sink.inner.into_vec()))
}

/// Import UTF-8 documentation without altering Markdown or line endings.
///
/// Title and body share both byte ceilings. Validation of documentation inputs
/// uses `Decode` errors, including invalid versions and missing titles.
pub fn import_documentation(
    document: LegacyDocumentation,
    options: CompatOptions,
) -> Result<Conversion<LegacyDocumentation>> {
    super::ensure_version(document.version, options)?;
    validate_documentation(
        &document.title,
        &document.body,
        options.max_output_bytes.min(options.limits.max_payload_bytes),
    )?;
    Ok(Conversion::exact(document))
}

/// Construct version-1 documentation, preserving the supplied UTF-8 bytes.
///
/// Both strings are input data, so invalid inputs use `Decode` errors just as
/// documentation import does. The combined size is bounded by the output limit.
pub fn export_documentation(
    title: &str,
    body: &str,
    options: CompatOptions,
) -> Result<LegacyDocumentation> {
    super::ensure_version(LEGACY_FORMAT_VERSION, options)?;
    validate_documentation(title, body, options.max_output_bytes)?;
    Ok(LegacyDocumentation {
        version: LEGACY_FORMAT_VERSION,
        title: copy_string(title, ErrorSource::Decode)?,
        body: copy_string(body, ErrorSource::Decode)?,
    })
}

fn schema_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaCompile, 0, detail)
}

fn schema_source(mut error: Error) -> Error {
    error.source = ErrorSource::SchemaCompile;
    error
}

fn schema_version(version: u16, options: CompatOptions) -> Result<()> {
    super::ensure_version(version, options).map_err(schema_source)
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<()> {
    values.try_reserve(additional).map_err(|_| {
        schema_error(ErrorCode::BudgetExhausted, "schema compatibility allocation failed")
    })
}

fn copy_string(value: &str, source: ErrorSource) -> Result<String> {
    let mut output = String::new();
    output.try_reserve_exact(value.len()).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            source,
            0,
            "compatibility string allocation failed",
        )
    })?;
    output.push_str(value);
    Ok(output)
}

fn validate_schema(schema: &SchemaIr) -> Result<()> {
    let context = |code, field_id, detail| {
        schema_error(code, detail).with_schema(schema.schema_id.0, field_id)
    };
    if schema.name.is_empty() {
        return Err(context(ErrorCode::MissingRequiredField, 0, "schema name is empty"));
    }

    // Sort borrowed fields, never the native vector. Scratch allocation is
    // fallible, and sorting avoids quadratic work on large untrusted schemas.
    let mut fields = Vec::new();
    reserve(&mut fields, schema.fields.len())?;
    let mut required_count = 0_u16;
    for field in &schema.fields {
        if field.name.is_empty() {
            return Err(context(
                ErrorCode::MissingRequiredField, field.field_id.0, "schema field name is empty",
            ));
        }
        if field.format.as_deref() == Some("") {
            return Err(context(
                ErrorCode::ConstraintViolation, field.field_id.0, "schema format name is empty",
            ));
        }
        if let (Some(minimum), Some(maximum)) = (field.min_len, field.max_len) {
            if minimum > maximum {
                return Err(context(
                    ErrorCode::ConstraintViolation, field.field_id.0,
                    "schema minimum length exceeds maximum length",
                ));
            }
        }
        if let (Some(minimum), Some(maximum)) = (field.min_i64, field.max_i64) {
            if minimum > maximum {
                return Err(context(
                    ErrorCode::ConstraintViolation, field.field_id.0,
                    "schema minimum integer exceeds maximum integer",
                ));
            }
        }
        if field.required {
            required_count = required_count.checked_add(1).ok_or_else(|| {
                context(
                    ErrorCode::ConstraintViolation, field.field_id.0,
                    "schema required field count exceeds u16",
                )
            })?;
        }
        fields.push(field);
    }
    if required_count != schema.required_count {
        return Err(context(
            ErrorCode::ConstraintViolation, 0,
            "schema required_count does not match required fields",
        ));
    }

    fields.sort_unstable_by_key(|field| field.field_id.0);
    for pair in fields.windows(2) {
        if pair[0].field_id == pair[1].field_id {
            return Err(context(
                ErrorCode::DuplicateObjectKey, pair[1].field_id.0,
                "duplicate schema field identifier",
            ));
        }
    }
    fields.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    for pair in fields.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(context(
                ErrorCode::DuplicateObjectKey, pair[1].field_id.0,
                "duplicate schema field name",
            ));
        }
    }
    Ok(())
}

fn copy_schema(schema: &SchemaIr) -> Result<SchemaIr> {
    let mut fields = Vec::new();
    reserve(&mut fields, schema.fields.len())?;
    for field in &schema.fields {
        fields.push(SchemaFieldIr {
            field_id: field.field_id,
            name: copy_string(&field.name, ErrorSource::SchemaCompile)?,
            required: field.required,
            scalar_kind: field.scalar_kind,
            format: field.format.as_deref()
                .map(|format| copy_string(format, ErrorSource::SchemaCompile))
                .transpose()?,
            min_len: field.min_len,
            max_len: field.max_len,
            min_i64: field.min_i64,
            max_i64: field.max_i64,
        });
    }
    Ok(SchemaIr {
        name: copy_string(&schema.name, ErrorSource::SchemaCompile)?,
        schema_id: schema.schema_id,
        required_count: schema.required_count,
        reject_unknown: schema.reject_unknown,
        fields,
    })
}

fn validate_documentation(title: &str, body: &str, maximum: u32) -> Result<()> {
    if title.is_empty() {
        return Err(Error::new(
            ErrorCode::MissingRequiredField, ErrorSource::Decode, 0,
            "documentation title is empty",
        ));
    }
    documentation_length(title.len(), body.len(), maximum)?;
    Ok(())
}

fn documentation_length(title: usize, body: usize, maximum: u32) -> Result<usize> {
    let total = title.checked_add(body).ok_or_else(|| {
        Error::new(
            ErrorCode::PayloadTooLarge, ErrorSource::Decode, 0,
            "documentation byte length overflow",
        )
    })?;
    if total > maximum as usize {
        return Err(Error::new(
            ErrorCode::PayloadTooLarge, ErrorSource::Decode, 0,
            "documentation byte limit exceeded",
        ));
    }
    Ok(total)
}

/// Retain the first sink failure even if an application codec ignores it.
/// All accumulated output remains in the root's bounded, fallible buffer.
struct SchemaSink {
    inner: BoundedSink,
    failure: Option<Error>,
}

impl SchemaSink {
    fn record(&mut self, result: Result<()>) -> Result<()> {
        result.map_err(|error| {
            let error = schema_source(error);
            self.failure = Some(error.clone());
            error
        })
    }
}

impl ByteSink for SchemaSink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.inner.reserve(additional);
        self.record(result)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.inner.write_bytes(bytes);
        self.record(result)
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use jmax_abi::{schema::{FieldId, SchemaId}, syntax::{Limits, ScalarKind}};

    #[derive(Clone, Copy)]
    enum EncodeMode { Normal, IgnoreWriteFailure, IgnoreReserveFailure }

    // A scripted application codec observes the adapter boundary without
    // inventing a legacy grammar. Decoder results and opaque encoder bytes are
    // explicit fixtures; every call records the complete input it receives.
    struct Codec {
        decoded: Option<Result<LegacySchema>>,
        encoded: Option<LegacySchema>,
        decode_calls: usize,
        encode_calls: usize,
        input: Vec<u8>,
        input_pointer: usize,
        limits: Option<Limits>,
        output: &'static [u8],
        encode_error: Option<Error>,
        mode: EncodeMode,
    }

    impl Codec {
        fn new(schema: SchemaIr) -> Self {
            Self {
                decoded: Some(Ok(LegacySchema {
                    version: LEGACY_FORMAT_VERSION, schema, description: String::new(),
                })),
                encoded: None,
                decode_calls: 0,
                encode_calls: 0,
                input: Vec::new(),
                input_pointer: 0,
                limits: None,
                output: b"old\0\xff\r\n",
                encode_error: None,
                mode: EncodeMode::Normal,
            }
        }

        fn legacy_mut(&mut self) -> &mut LegacySchema {
            self.decoded.as_mut().unwrap().as_mut().unwrap()
        }
    }

    impl LegacySchemaCodec for Codec {
        fn decode_schema(&mut self, input: &[u8], limits: Limits) -> Result<LegacySchema> {
            self.decode_calls += 1;
            self.input = input.to_vec();
            self.input_pointer = input.as_ptr() as usize;
            self.limits = Some(limits);
            self.decoded.take().expect("one scripted decode per codec")
        }

        fn encode_schema(&mut self, schema: &LegacySchema, sink: &mut dyn ByteSink) -> Result<()> {
            self.encode_calls += 1;
            self.encoded = Some(schema.clone());
            match self.mode {
                EncodeMode::Normal => {
                    let before = sink.len();
                    sink.reserve(self.output.len())?;
                    assert_eq!(sink.len(), before);
                    for chunk in self.output.chunks(2) {
                        sink.write_bytes(chunk)?;
                    }
                    assert_eq!(sink.len(), before + self.output.len());
                }
                EncodeMode::IgnoreWriteFailure => {
                    let _ = sink.write_bytes(self.output);
                    let _ = sink.write_bytes(b"!");
                    let _ = sink.write_bytes(b"");
                }
                EncodeMode::IgnoreReserveFailure => {
                    sink.write_bytes(b"x")?;
                    let _ = sink.reserve(usize::MAX);
                    let _ = sink.reserve(0);
                    let _ = sink.write_bytes(b"");
                }
            }
            if let Some(error) = &self.encode_error {
                return Err(error.clone());
            }
            Ok(())
        }
    }

    fn field(id: u32, name: &str, required: bool) -> SchemaFieldIr {
        SchemaFieldIr {
            field_id: FieldId(id), name: name.into(), required,
            scalar_kind: None, format: None,
            min_len: None, max_len: None, min_i64: None, max_i64: None,
        }
    }

    fn schema() -> SchemaIr {
        let mut text = field(u32::MAX, "Labelé", true);
        text.scalar_kind = Some(ScalarKind::String);
        text.format = Some("application/CustomFormat".into());
        text.min_len = Some(0);
        text.max_len = Some(u32::MAX);
        let mut integer = field(0, "quantity", false);
        integer.scalar_kind = Some(ScalarKind::Number);
        integer.min_i64 = Some(i64::MIN);
        integer.max_i64 = Some(i64::MAX);
        SchemaIr {
            name: "Tradeé\r\n".into(), schema_id: SchemaId(u32::MAX),
            required_count: 1, reject_unknown: true,
            fields: vec![text, integer, field(17, "labelé", false)],
        }
    }

    fn same_schema(actual: &SchemaIr, expected: &SchemaIr) {
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.schema_id, expected.schema_id);
        assert_eq!(actual.required_count, expected.required_count);
        assert_eq!(actual.reject_unknown, expected.reject_unknown);
        assert_eq!(actual.fields.len(), expected.fields.len());
        for (actual, expected) in actual.fields.iter().zip(&expected.fields) {
            assert_eq!(actual.field_id, expected.field_id);
            assert_eq!(actual.name, expected.name);
            assert_eq!(actual.required, expected.required);
            assert_eq!(actual.scalar_kind, expected.scalar_kind);
            assert_eq!(actual.format, expected.format);
            assert_eq!((actual.min_len, actual.max_len), (expected.min_len, expected.max_len));
            assert_eq!((actual.min_i64, actual.max_i64), (expected.min_i64, expected.max_i64));
        }
    }

    fn assert_error<T: core::fmt::Debug>(
        result: Result<T>, code: ErrorCode, source: ErrorSource,
    ) -> Error {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert!(!error.detail.is_empty());
        error
    }

    fn assert_invalid(model: SchemaIr, code: ErrorCode) {
        for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
            let options = CompatOptions { loss_policy, ..CompatOptions::default() };
            let mut codec = Codec::new(model.clone());
            // Bad IR must fail independently of permission to discard metadata.
            codec.legacy_mut().description = "metadata".into();
            let imported = assert_error(
                import_schema(b"fixture", &mut codec, options), code, ErrorSource::SchemaCompile,
            );
            assert_eq!(imported.schema_id, model.schema_id.0);
            assert_eq!(imported.byte_offset, 0);
            let mut codec = Codec::new(schema());
            let exported = assert_error(
                export_schema(&model, "metadata", &mut codec, options),
                code, ErrorSource::SchemaCompile,
            );
            assert_eq!(exported.schema_id, model.schema_id.0);
            assert_eq!(exported.byte_offset, 0);
            assert_eq!(codec.encode_calls, 0);
        }
    }

    fn document(title: &str, body: &str) -> LegacyDocumentation {
        LegacyDocumentation {
            version: LEGACY_FORMAT_VERSION, title: title.into(), body: body.into(),
        }
    }

    #[test]
    fn import_preserves_every_ir_field_order_owner_and_supplied_limits() {
        let expected = schema();
        let input = b"application grammar\r\nremaining bytes";
        for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
            let mut codec = Codec::new(expected.clone());
            let owner = codec.legacy_mut().schema.fields.as_ptr();
            let name_owner = codec.legacy_mut().schema.name.as_ptr();
            let options = CompatOptions {
                loss_policy, max_output_bytes: 0,
                limits: Limits { max_depth: 7, max_decoded_bytes: 42, ..Limits::strict_default() },
                ..CompatOptions::default()
            };
            let imported = import_schema(input, &mut codec, options).unwrap();
            assert!(imported.is_lossless());
            same_schema(&imported.value, &expected);
            assert_eq!(imported.value.fields.as_ptr(), owner);
            assert_eq!(imported.value.name.as_ptr(), name_owner);
            assert_eq!(codec.input, input);
            assert_eq!(codec.input_pointer, input.as_ptr() as usize);
            assert_eq!(codec.limits, Some(options.limits));
            assert_eq!((codec.decode_calls, codec.encode_calls), (1, 0));
        }
    }

    #[test]
    fn export_preserves_description_and_opaque_bytes_without_json_limits() {
        let original = schema();
        let expected = original.clone();
        let description = "# Legacy\r\n*é*\0";
        for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
            let mut codec = Codec::new(schema());
            let options = CompatOptions {
                loss_policy, max_output_bytes: codec.output.len() as u32,
                limits: Limits {
                    max_payload_bytes: 0, max_decoded_bytes: 0, max_depth: 0,
                    max_fields_per_object: 0, max_string_bytes: 0,
                    ..Limits::strict_default()
                },
                ..CompatOptions::default()
            };
            let payload = export_schema(&original, description, &mut codec, options).unwrap();
            assert_eq!(payload.as_slice(), codec.output);
            let exported = codec.encoded.as_ref().unwrap();
            assert_eq!(exported.version, LEGACY_FORMAT_VERSION);
            assert_eq!(exported.description.as_bytes(), description.as_bytes());
            same_schema(&exported.schema, &expected);
            assert_ne!(exported.schema.name.as_ptr(), original.name.as_ptr());
            assert_ne!(exported.schema.fields.as_ptr(), original.fields.as_ptr());
            assert_ne!(exported.schema.fields[0].name.as_ptr(), original.fields[0].name.as_ptr());
            assert_eq!((codec.decode_calls, codec.encode_calls), (0, 1));
            codec.encoded.as_mut().unwrap().schema.fields[0].format.as_mut().unwrap().clear();
            same_schema(&original, &expected);
        }
    }

    #[test]
    fn description_loss_has_exact_warning_and_never_silently_drops_whitespace() {
        for description in ["description", " ", "\r\n", "é\0*markdown*"] {
            let mut codec = Codec::new(schema());
            codec.legacy_mut().description = description.into();
            let error = assert_error(
                import_schema(b"fixture", &mut codec, CompatOptions::default()),
                ErrorCode::UnsupportedSchemaFeature, ErrorSource::SchemaCompile,
            );
            assert_eq!(error.schema_id, u32::MAX);
            let mut codec = Codec::new(schema());
            codec.legacy_mut().description = description.into();
            let result = import_schema(b"fixture", &mut codec, CompatOptions {
                loss_policy: LossPolicy::AllowDocumented, ..CompatOptions::default()
            }).unwrap();
            same_schema(&result.value, &schema());
            assert!(!result.is_lossless());
            assert_eq!(result.warnings, vec![CompatibilityWarning {
                path: "description".into(),
                detail: "legacy schema description is not represented by SchemaIr".into(),
            }]);
            let mapped = result.map(|model| model.schema_id);
            assert_eq!(mapped.value, SchemaId(u32::MAX));
            assert_eq!(mapped.warnings.len(), 1);
        }
    }

    #[test]
    fn unsupported_option_versions_fail_all_adapters_before_codec_calls() {
        for version in [0, 2, 255, u16::MAX] {
            for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
                let options = CompatOptions {
                    legacy_version: version, loss_policy, ..CompatOptions::default()
                };
                let mut codec = Codec::new(schema());
                assert_error(import_schema(b"", &mut codec, options),
                    ErrorCode::UnsupportedSchemaFeature, ErrorSource::SchemaCompile);
                assert_error(export_schema(&schema(), "", &mut codec, options),
                    ErrorCode::UnsupportedSchemaFeature, ErrorSource::SchemaCompile);
                assert_eq!((codec.decode_calls, codec.encode_calls), (0, 0));
                assert_error(import_documentation(document("T", ""), options),
                    ErrorCode::UnsupportedSchemaFeature, ErrorSource::Decode);
                assert_error(export_documentation("T", "", options),
                    ErrorCode::UnsupportedSchemaFeature, ErrorSource::Decode);
            }
        }
    }

    #[test]
    fn codec_and_document_wrapper_versions_cannot_override_options() {
        for version in [0, 2, 255, u16::MAX] {
            for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
                let options = CompatOptions { loss_policy, ..CompatOptions::default() };
                let mut codec = Codec::new(schema());
                codec.legacy_mut().version = version;
                assert_error(import_schema(b"fixture", &mut codec, options),
                    ErrorCode::UnsupportedSchemaFeature, ErrorSource::SchemaCompile);
                assert_eq!(codec.decode_calls, 1);
                let mut value = document("Title", "body");
                value.version = version;
                assert_error(import_documentation(value, options),
                    ErrorCode::UnsupportedSchemaFeature, ErrorSource::Decode);
            }
        }
    }

    #[test]
    fn empty_schema_and_field_names_are_required_without_trimming() {
        let mut value = schema(); value.name.clear();
        assert_invalid(value, ErrorCode::MissingRequiredField);
        let mut value = schema(); value.fields[0].name.clear();
        assert_invalid(value, ErrorCode::MissingRequiredField);
        let mut value = schema(); value.name = " ".into(); value.fields[0].name = "\t".into();
        let mut codec = Codec::new(value.clone());
        same_schema(&import_schema(b"fixture", &mut codec, CompatOptions::default()).unwrap().value, &value);
    }

    #[test]
    fn duplicate_ids_and_names_are_rejected_even_when_nonadjacent_and_permissive() {
        let mut duplicate_id = schema();
        duplicate_id.fields[2].field_id = duplicate_id.fields[0].field_id;
        assert_invalid(duplicate_id.clone(), ErrorCode::DuplicateObjectKey);
        let mut duplicate_name = schema();
        duplicate_name.fields[2].name = duplicate_name.fields[0].name.clone();
        assert_invalid(duplicate_name.clone(), ErrorCode::DuplicateObjectKey);
        for value in [duplicate_id, duplicate_name] {
            let options = CompatOptions { limits: Limits::permissive_default(), ..CompatOptions::default() };
            let mut codec = Codec::new(value.clone());
            assert_error(import_schema(b"fixture", &mut codec, options),
                ErrorCode::DuplicateObjectKey, ErrorSource::SchemaCompile);
            assert_error(export_schema(&value, "", &mut codec, options),
                ErrorCode::DuplicateObjectKey, ErrorSource::SchemaCompile);
        }
    }

    #[test]
    fn required_counts_are_validated_without_repairing_native_metadata() {
        for count in [0, 2, u16::MAX] {
            let mut value = schema(); value.required_count = count;
            assert_invalid(value, ErrorCode::ConstraintViolation);
        }
        let mut value = schema(); value.fields[1].required = true;
        assert_invalid(value, ErrorCode::ConstraintViolation);
        let value = SchemaIr { name: "Empty".into(), ..SchemaIr::default() };
        let mut codec = Codec::new(value.clone());
        let result = import_schema(b"", &mut codec, CompatOptions::default()).unwrap();
        assert!(result.is_lossless());
        same_schema(&result.value, &value);
        let mut bad = value; bad.required_count = 1;
        assert_invalid(bad, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn full_u16_required_count_boundary_and_one_past_it_are_checked() {
        let mut value = SchemaIr {
            name: "Many".into(), schema_id: SchemaId(19), required_count: u16::MAX,
            reject_unknown: false,
            fields: (0..u32::from(u16::MAX))
                .map(|id| field(id, &format!("f{id}"), true)).collect(),
        };
        validate_schema(&value).unwrap();
        let mut codec = Codec::new(value.clone());
        let imported = import_schema(b"fixture", &mut codec, CompatOptions::default()).unwrap();
        same_schema(&imported.value, &value);
        export_schema(&value, "", &mut codec, CompatOptions::default()).unwrap();
        value.fields.push(field(u32::from(u16::MAX), "one_more", true));
        assert_invalid(value.clone(), ErrorCode::ConstraintViolation);
        // The u16 ceiling applies to required fields, not total fields or IDs.
        for item in &mut value.fields { item.required = false; }
        value.required_count = 0;
        validate_schema(&value).unwrap();
        assert_eq!(value.fields.len(), usize::from(u16::MAX) + 1);
    }

    #[test]
    fn inverted_length_integer_and_empty_format_constraints_fail_with_field_context() {
        let mut value = schema(); value.fields[0].min_len = Some(1); value.fields[0].max_len = Some(0);
        assert_invalid(value, ErrorCode::ConstraintViolation);
        let mut value = schema(); value.fields[1].min_i64 = Some(i64::MAX); value.fields[1].max_i64 = Some(i64::MIN);
        assert_invalid(value, ErrorCode::ConstraintViolation);
        let mut value = schema(); value.fields[0].format = Some(String::new());
        assert_invalid(value.clone(), ErrorCode::ConstraintViolation);
        let error = validate_schema(&value).unwrap_err();
        assert_eq!((error.schema_id, error.field_id), (u32::MAX, u32::MAX));
    }

    #[test]
    fn inclusive_and_single_sided_bounds_preserve_extreme_values() {
        for (min_len, max_len) in [
            (None, None), (Some(0), Some(0)), (Some(u32::MAX), Some(u32::MAX)),
            (Some(u32::MAX), None), (None, Some(0)),
        ] {
            for (min_i64, max_i64) in [
                (None, None), (Some(i64::MIN), Some(i64::MIN)),
                (Some(i64::MAX), Some(i64::MAX)), (Some(i64::MAX), None),
                (None, Some(i64::MIN)), (Some(i64::MIN), Some(i64::MAX)),
            ] {
                let mut value = schema();
                let item = &mut value.fields[2];
                item.min_len = min_len; item.max_len = max_len;
                item.min_i64 = min_i64; item.max_i64 = max_i64;
                let mut codec = Codec::new(value.clone());
                let imported = import_schema(b"fixture", &mut codec, CompatOptions::default()).unwrap();
                same_schema(&imported.value, &value);
                export_schema(&value, "", &mut codec, CompatOptions::default()).unwrap();
                same_schema(&codec.encoded.as_ref().unwrap().schema, &value);
            }
        }
    }

    #[test]
    fn scalar_tags_and_arbitrary_nonempty_formats_are_not_rewritten() {
        for scalar_kind in [None, Some(ScalarKind::String), Some(ScalarKind::Number),
            Some(ScalarKind::True), Some(ScalarKind::False), Some(ScalarKind::Null)]
        {
            for format in [None, Some(" "), Some("Custom-é"), Some("int64")] {
                let mut value = schema();
                value.fields[2].scalar_kind = scalar_kind;
                value.fields[2].format = format.map(String::from);
                let mut codec = Codec::new(value.clone());
                let imported = import_schema(b"fixture", &mut codec, CompatOptions::default()).unwrap();
                same_schema(&imported.value, &value);
                export_schema(&value, "", &mut codec, CompatOptions::default()).unwrap();
                same_schema(&codec.encoded.as_ref().unwrap().schema, &value);
            }
        }
    }

    #[test]
    fn schema_input_ceiling_is_inclusive_and_precedes_codec_invocation() {
        for maximum in [0, 1, 4] {
            let mut options = CompatOptions { max_output_bytes: 0, ..CompatOptions::default() };
            options.limits.max_payload_bytes = maximum;
            let input = vec![b'x'; maximum as usize];
            let mut codec = Codec::new(schema());
            import_schema(&input, &mut codec, options).unwrap();
            assert_eq!(codec.input, input);
            let oversized = vec![b'x'; maximum as usize + 1];
            let mut codec = Codec::new(schema());
            assert_error(import_schema(&oversized, &mut codec, options),
                ErrorCode::PayloadTooLarge, ErrorSource::SchemaCompile);
            assert_eq!(codec.decode_calls, 0);
        }
    }

    #[test]
    fn codec_failures_preserve_code_location_context_and_detail() {
        for code in [
            ErrorCode::UnsupportedSchemaFeature, ErrorCode::InvalidUtf8,
            ErrorCode::TrailingContent, ErrorCode::DepthLimitExceeded,
            ErrorCode::DecodedArenaOverflow, ErrorCode::BudgetExhausted,
            ErrorCode::Cancelled,
        ] {
            for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
                let original = Error::new(code, ErrorSource::Lexer, 23, "codec rejected artifact")
                    .with_schema(5, 9);
                let expected = schema_source(original.clone());
                let mut codec = Codec::new(schema());
                codec.decoded = Some(Err(original.clone()));
                let options = CompatOptions { loss_policy, ..CompatOptions::default() };
                assert_eq!(import_schema(b"\xff source", &mut codec, options).unwrap_err(), expected);
                assert_eq!(codec.input, b"\xff source");
                codec.encode_error = Some(original);
                assert_eq!(export_schema(&schema(), "", &mut codec, options).unwrap_err(), expected);
                assert_eq!(codec.encode_calls, 1);
            }
        }
    }

    #[test]
    fn output_ceiling_accepts_exact_and_empty_artifacts_and_rejects_excess() {
        for output in [&b""[..], &b"x"[..], &b"abcd"[..], &b"\0\xff\r\n"[..]] {
            let mut codec = Codec::new(schema()); codec.output = output;
            let options = CompatOptions { max_output_bytes: output.len() as u32, ..CompatOptions::default() };
            assert_eq!(export_schema(&schema(), "", &mut codec, options).unwrap().as_slice(), output);
            if !output.is_empty() {
                let options = CompatOptions { max_output_bytes: output.len() as u32 - 1, ..options };
                assert_error(export_schema(&schema(), "", &mut codec, options),
                    ErrorCode::PayloadTooLarge, ErrorSource::SchemaCompile);
            }
        }
        let mut codec = Codec::new(schema());
        let options = CompatOptions { max_output_bytes: u32::MAX, ..CompatOptions::default() };
        assert_eq!(export_schema(&schema(), "", &mut codec, options).unwrap().as_slice(), codec.output);
    }

    #[test]
    fn ignored_or_replaced_sink_write_errors_cannot_publish_a_prefix() {
        for replacement in [None, Some(Error::new(
            ErrorCode::InvalidSyntax, ErrorSource::Syntax, 41, "replacement error",
        ))] {
            let mut codec = Codec::new(schema());
            codec.mode = EncodeMode::IgnoreWriteFailure;
            codec.output = b"too large";
            codec.encode_error = replacement;
            assert_error(export_schema(&schema(), "", &mut codec, CompatOptions {
                max_output_bytes: 1, ..CompatOptions::default()
            }), ErrorCode::PayloadTooLarge, ErrorSource::SchemaCompile);
        }
    }

    #[test]
    fn overflowing_reservations_remain_errors_after_ignored_empty_operations() {
        let mut codec = Codec::new(schema()); codec.mode = EncodeMode::IgnoreReserveFailure;
        codec.encode_error = Some(Error::new(
            ErrorCode::Cancelled, ErrorSource::Encode, 0, "codec replaced sink error",
        ));
        let error = assert_error(export_schema(&schema(), "", &mut codec, CompatOptions {
            max_output_bytes: 1, ..CompatOptions::default()
        }), ErrorCode::PayloadTooLarge, ErrorSource::SchemaCompile);
        assert_eq!(error.detail, "compatibility output length overflow");
    }

    #[test]
    fn sink_retains_first_failure_and_unchanged_storage_after_failure() {
        let mut sink = SchemaSink { inner: BoundedSink::new(2), failure: None };
        sink.reserve(2).unwrap();
        assert_eq!(sink.len(), 0);
        sink.write_bytes(b"a").unwrap();
        let first = sink.reserve(usize::MAX).unwrap_err();
        assert_eq!(first.code, ErrorCode::PayloadTooLarge);
        assert_eq!(first.source, ErrorSource::SchemaCompile);
        assert_eq!(sink.write_bytes(b"b").unwrap_err(), first);
        assert_eq!(sink.write_bytes(b"").unwrap_err(), first);
        assert_eq!(sink.reserve(0).unwrap_err(), first);
        assert_eq!(sink.inner.as_slice(), b"a");
        assert_eq!(sink.len(), 1);
        assert_eq!(sink.failure.as_ref(), Some(&first));
    }

    #[test]
    fn sink_checks_each_write_after_reservation_at_the_inclusive_ceiling() {
        let mut sink = SchemaSink { inner: BoundedSink::new(3), failure: None };
        sink.reserve(3).unwrap();
        sink.write_bytes(b"a").unwrap();
        sink.reserve(2).unwrap();
        assert_eq!(sink.len(), 1);
        sink.write_bytes(b"bc").unwrap();
        sink.write_bytes(b"").unwrap();
        assert_eq!(sink.inner.as_slice(), b"abc");
        let error = sink.write_bytes(b"d").unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(sink.inner.as_slice(), b"abc");
        assert_eq!(sink.len(), 3);
        assert_eq!(sink.failure.as_ref(), Some(&error));
    }

    #[test]
    fn documentation_round_trip_preserves_utf8_markdown_controls_and_line_endings() {
        for (title, body) in [
            ("Title", ""), ("é猫", "# Heading\r\n*bold* `code` [x](url)\n"),
            (" \t", "\0\t\r\n leading and trailing  "),
            ("e\u{301}", "é is distinct from e\u{301}"),
        ] {
            for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
                let options = CompatOptions { loss_policy, ..CompatOptions::default() };
                let exported = export_documentation(title, body, options).unwrap();
                assert_eq!(exported, document(title, body));
                let title_owner = exported.title.as_ptr();
                let body_owner = exported.body.as_ptr();
                let result = import_documentation(exported, options).unwrap();
                assert!(result.is_lossless());
                assert_eq!(result.value.title.as_bytes(), title.as_bytes());
                assert_eq!(result.value.body.as_bytes(), body.as_bytes());
                assert_eq!(result.value.title.as_ptr(), title_owner);
                assert_eq!(result.value.body.as_ptr(), body_owner);
            }
        }
    }

    #[test]
    fn documentation_limits_count_combined_utf8_bytes_at_both_import_ceilings() {
        let title = "é";
        let body = "猫";
        assert_eq!(title.len() + body.len(), 5);
        let options = CompatOptions {
            max_output_bytes: 5,
            limits: Limits { max_payload_bytes: 5, ..Limits::strict_default() },
            ..CompatOptions::default()
        };
        let exported = export_documentation(title, body, options).unwrap();
        assert_eq!(import_documentation(exported, options).unwrap().value, document(title, body));
        let output_tight = CompatOptions { max_output_bytes: 4, ..options };
        assert_error(export_documentation(title, body, output_tight),
            ErrorCode::PayloadTooLarge, ErrorSource::Decode);
        assert_error(import_documentation(document(title, body), output_tight),
            ErrorCode::PayloadTooLarge, ErrorSource::Decode);
        let payload_tight = CompatOptions {
            limits: Limits { max_payload_bytes: 4, ..options.limits }, ..options
        };
        assert_error(import_documentation(document(title, body), payload_tight),
            ErrorCode::PayloadTooLarge, ErrorSource::Decode);
        // Export creates an in-memory wrapper and has no payload-input ceiling.
        assert_eq!(export_documentation(title, body, payload_tight).unwrap(), document(title, body));
    }

    #[test]
    fn documentation_requires_a_title_and_respects_zero_byte_ceilings() {
        for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
            let options = CompatOptions { loss_policy, max_output_bytes: 0, ..CompatOptions::default() };
            for body in ["", "body"] {
                assert_error(import_documentation(document("", body), options),
                    ErrorCode::MissingRequiredField, ErrorSource::Decode);
                assert_error(export_documentation("", body, options),
                    ErrorCode::MissingRequiredField, ErrorSource::Decode);
            }
            assert_error(import_documentation(document("T", ""), options),
                ErrorCode::PayloadTooLarge, ErrorSource::Decode);
            assert_error(export_documentation("T", "", options),
                ErrorCode::PayloadTooLarge, ErrorSource::Decode);
            let options = CompatOptions {
                max_output_bytes: 1,
                limits: Limits { max_payload_bytes: 0, ..options.limits }, ..options
            };
            assert_error(import_documentation(document("T", ""), options),
                ErrorCode::PayloadTooLarge, ErrorSource::Decode);
            assert_eq!(export_documentation("T", "", options).unwrap(), document("T", ""));
        }
    }

    #[test]
    fn documentation_checked_arithmetic_handles_abi_and_machine_boundaries() {
        assert_eq!(documentation_length(0, 0, 0).unwrap(), 0);
        assert_eq!(documentation_length(1, 2, 3).unwrap(), 3);
        assert_eq!(documentation_length(u32::MAX as usize, 0, u32::MAX).unwrap(), u32::MAX as usize);
        assert_error(documentation_length(usize::MAX, 1, u32::MAX),
            ErrorCode::PayloadTooLarge, ErrorSource::Decode);
        assert_error(documentation_length(1, usize::MAX, u32::MAX),
            ErrorCode::PayloadTooLarge, ErrorSource::Decode);
        assert_error(documentation_length(u32::MAX as usize, 1, u32::MAX),
            ErrorCode::PayloadTooLarge, ErrorSource::Decode);
        assert_error(documentation_length(2, 2, 3),
            ErrorCode::PayloadTooLarge, ErrorSource::Decode);
    }

    #[test]
    fn fallible_capacity_overflow_is_canonical_and_preserves_existing_values() {
        let mut values = vec![7_u8];
        let error = assert_error(reserve(&mut values, usize::MAX),
            ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile);
        assert_eq!(values, [7]);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert_eq!(copy_string("é\r\n", ErrorSource::SchemaCompile).unwrap(), "é\r\n");
    }

    #[test]
    fn schema_export_import_boundary_round_trip_documents_only_description_loss() {
        let expected = schema();
        let mut encoder = Codec::new(schema());
        let payload = export_schema(&expected, "legacy docs", &mut encoder, CompatOptions::default()).unwrap();
        let mut decoder = Codec::new(schema());
        decoder.decoded = Some(Ok(encoder.encoded.take().unwrap()));
        let result = import_schema(payload.as_slice(), &mut decoder, CompatOptions {
            loss_policy: LossPolicy::AllowDocumented, ..CompatOptions::default()
        }).unwrap();
        same_schema(&result.value, &expected);
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.warnings[0].path, "description");
        assert_eq!(decoder.input, encoder.output);
    }
}
