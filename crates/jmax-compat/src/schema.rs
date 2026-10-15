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
