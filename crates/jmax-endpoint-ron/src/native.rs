//! Loading of the separate native decoder specification (`native.ron`).
//!
//! The document has no connection to a model here: names are resolved later by
//! `jmax_endpoint_model::native::validate_native_spec`.

use alloc::string::String;
use jmax_abi::number::DecimalPolicy;
use jmax_endpoint_model::native::{NanoIdAlphabet, NativeIdent, NativeNanoIdField, NativeSpec};

use crate::normalize::{at, boolean, list, name, record, text, unsigned};
use crate::{ConfigLimits, ConfigNode, ConfigValue, ErrorCode, Result};

/// Parse and normalize one native specification document.
pub fn load_native_spec(source: &str, limits: &ConfigLimits) -> Result<NativeSpec> {
    let loaded = crate::load::parse_str(source, "<native>", limits)?;
    normalize_native_spec(&loaded.root)
}

fn normalize_native_spec(node: &ConfigNode) -> Result<NativeSpec> {
    let record = record(node, &["NativeSpec", "Native"], &[
        "roots", "tuples", "decimal", "reject_unknown", "idents", "nanoid_alphabet", "nanoid_fields",
        "nullable_optionals",
    ])?;
    let defaults = NativeSpec::default();
    Ok(NativeSpec {
        roots: list(record.get("roots"), name)?,
        tuples: list(record.get("tuples"), name)?,
        decimal_policy: record.get("decimal").map(decimal).transpose()?
            .unwrap_or(defaults.decimal_policy),
        reject_unknown: record.get("reject_unknown").map(boolean).transpose()?
            .unwrap_or(defaults.reject_unknown),
        idents: list(record.get("idents"), ident)?,
        nanoid_alphabet: record.get("nanoid_alphabet").map(alphabet).transpose()?
            .unwrap_or(defaults.nanoid_alphabet),
        nanoid_fields: list(record.get("nanoid_fields"), nanoid_field)?,
        nullable_optionals: record.get("nullable_optionals").map(boolean).transpose()?
            .unwrap_or(defaults.nullable_optionals),
    })
}

fn alphabet(node: &ConfigNode) -> Result<NanoIdAlphabet> {
    match &node.value {
        ConfigValue::Identifier(tag) if tag == "Url" => Ok(NanoIdAlphabet::Url),
        ConfigValue::Identifier(tag) if tag == "Base62" => Ok(NanoIdAlphabet::Base62),
        _ => Err(at(node, ErrorCode::TypeMismatch, "expected the alphabet Url or Base62")),
    }
}

/// `NanoIdField("Type", "wire", Base62)` or `NanoIdField(type: .., wire: .., alphabet: ..)`.
fn nanoid_field(node: &ConfigNode) -> Result<NativeNanoIdField> {
    if let ConfigValue::Variant { name: tag, value } = &node.value {
        if tag == "NanoIdField" {
            if let ConfigValue::Tuple(items) = &value.value {
                let [type_name, wire, alphabet_node] = items.as_slice() else {
                    return Err(at(node, ErrorCode::TypeMismatch, "NanoIdField requires type, wire and alphabet"));
                };
                return Ok(NativeNanoIdField {
                    type_name: name(type_name)?,
                    wire: wire_name(wire)?,
                    alphabet: alphabet(alphabet_node)?,
                });
            }
        }
    }
    let record = record(node, &["NanoIdField"], &["type", "wire", "alphabet"])?;
    Ok(NativeNanoIdField {
        type_name: name(record.required("type")?)?,
        wire: wire_name(record.required("wire")?)?,
        alphabet: alphabet(record.required("alphabet")?)?,
    })
}

fn decimal(node: &ConfigNode) -> Result<DecimalPolicy> {
    let record = record(node, &["Decimal"], &[
        "max_precision", "target_scale", "allow_exponent", "reject_inexact_rescale", "tick_step",
    ])?;
    let base = DecimalPolicy::spot_8();
    let tick_step = match record.get("tick_step") {
        None => base.tick_step,
        Some(node) => match &node.value {
            ConfigValue::U64(value) => i128::from(*value),
            ConfigValue::I64(value) => i128::from(*value),
            _ => return Err(at(node, ErrorCode::TypeMismatch, "expected an integer")),
        },
    };
    Ok(DecimalPolicy {
        max_precision: record.get("max_precision").map(unsigned).transpose()?
            .unwrap_or(base.max_precision),
        target_scale: record.get("target_scale").map(unsigned).transpose()?
            .unwrap_or(base.target_scale),
        allow_exponent: record.get("allow_exponent").map(boolean).transpose()?
            .unwrap_or(base.allow_exponent),
        reject_inexact_rescale: record.get("reject_inexact_rescale").map(boolean).transpose()?
            .unwrap_or(base.reject_inexact_rescale),
        tick_step,
    })
}

fn ident(node: &ConfigNode) -> Result<NativeIdent> {
    if let ConfigValue::Variant { name: tag, value } = &node.value {
        if tag == "Ident" {
            if let ConfigValue::Tuple(items) = &value.value {
                let [type_name, wire, ident] = items.as_slice() else {
                    return Err(at(node, ErrorCode::TypeMismatch, "Ident requires type, wire and ident"));
                };
                return Ok(NativeIdent {
                    type_name: name(type_name)?,
                    wire: wire_name(wire)?,
                    ident: name(ident)?,
                });
            }
        }
    }
    let record = record(node, &["Ident"], &["type", "wire", "ident"])?;
    Ok(NativeIdent {
        type_name: name(record.required("type")?)?,
        wire: wire_name(record.required("wire")?)?,
        ident: name(record.required("ident")?)?,
    })
}

/// Wire names may be any nonempty UTF-8 string.
fn wire_name(node: &ConfigNode) -> Result<String> {
    let value = text(node)?;
    if value.is_empty() {
        return Err(at(node, ErrorCode::ConstraintViolation, "wire name must not be empty"));
    }
    Ok(value)
}
