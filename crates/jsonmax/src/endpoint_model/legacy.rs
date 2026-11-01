//! Loss-checked conversion between the supported legacy model shapes.
//!
//! The document variant selects the shape; `ModelVersion` is preserved model
//! metadata, not a legacy format selector. Parsing and rejection of unsupported
//! future document versions belong to the importing parser.

use super::registry::build_registry;
use super::{
    model_error, EndpointModel, ErrorCode, LegacyDocument, LegacyModelV1,
    LegacyVersion, Result, RoleSpec,
};
use alloc::{string::String, vec::Vec};

/// Import a legacy document, validating the complete resulting model.
///
/// V1 role names become roles with empty includes and descriptions, and its
/// absent errors and headers become empty lists. V2 already has the canonical
/// shape. All existing values and declaration order are preserved in either
/// case. Registry validation errors are returned without modification.
pub fn import_legacy(document: LegacyDocument) -> Result<EndpointModel> {
    let model = match document {
        LegacyDocument::V1(legacy) => EndpointModel {
            version: legacy.version,
            services: legacy.services,
            definitions: legacy.definitions,
            roles: legacy.roles.into_iter().map(|name| RoleSpec {
                name,
                includes: Vec::new(),
                description: String::new(),
            }).collect(),
            errors: Vec::new(),
            headers: Vec::new(),
        },
        LegacyDocument::V2(model) => model,
    };
    build_registry(&model)?;
    Ok(model)
}

/// Validate and export a model without discarding information.
///
/// V1 cannot represent errors, headers, role inheritance, or role descriptions.
/// Any such metadata, including a whitespace-only description, produces
/// `UnsupportedSchemaFeature`. Validation always precedes this compatibility
/// decision. V2 clones the entire model exactly. Neither path mutates the input.
pub fn export_legacy(model: &EndpointModel, version: LegacyVersion) -> Result<LegacyDocument> {
    build_registry(model)?;
    match version {
        LegacyVersion::V1 => {
            if !model.errors.is_empty() {
                return Err(model_error(
                    ErrorCode::UnsupportedSchemaFeature,
                    "legacy V1 cannot represent error declarations",
                ));
            }
            if !model.headers.is_empty() {
                return Err(model_error(
                    ErrorCode::UnsupportedSchemaFeature,
                    "legacy V1 cannot represent header declarations",
                ));
            }
            if model.roles.iter().any(|role| !role.includes.is_empty() || !role.description.is_empty()) {
                return Err(model_error(
                    ErrorCode::UnsupportedSchemaFeature,
                    "legacy V1 cannot represent role includes or descriptions",
                ));
            }
            Ok(LegacyDocument::V1(LegacyModelV1 {
                version: model.version,
                services: model.services.clone(),
                definitions: model.definitions.clone(),
                roles: model.roles.iter().map(|role| role.name.clone()).collect(),
            }))
        }
        LegacyVersion::V2 => Ok(LegacyDocument::V2(model.clone())),
    }
}
