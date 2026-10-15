//! Loss-checked legacy artifacts and the DefinitionVariant metadata bridge.
//!
//! Legacy modes expose model declarations under their original model spelling
//! through re-exports. Nominal owner views use that spelling followed by `View`.
//! Aliases target the actual model declaration, including scalar aliases; they
//! never expand references, flatten service names, or change wire values.

use alloc::{collections::BTreeMap, format, string::String};

use crate::{codegen_error, rust_string_literal, validate_options, ArtifactKind,
    CodegenOptions, CompatibilityMode, EndpointModel, EndpointType, EnumVariantSpec,
    ErrorCode, GeneratedArtifact, Result, GENERATED_NOTICE};
use jmax_endpoint_model::{LegacyVersion, NameCase};

const MAX_DEPTH: usize = 128;
const VARIANTS_TYPE: &str = "&[(&'static str, &'static [(&'static str, i32, &'static str)])]";

fn constraint(detail: &'static str) -> crate::Error {
    codegen_error(ErrorCode::ConstraintViolation, detail)
}

fn check_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(codegen_error(ErrorCode::DepthLimitExceeded,
            "compatibility type nesting exceeds 128"));
    }
    Ok(())
}

fn pascal(name: &str) -> Result<String> {
    let name = jmax_endpoint_model::wire_names::convert_case(name, NameCase::Pascal)?;
    jmax_schema_aot::validate_symbol(&name)?;
    Ok(name)
}

/// Original model names have already passed the registry's identifier grammar.
/// Raw identifiers preserve legal keyword spellings without treating names as
/// expressions. Rust forbids these five spellings even as raw identifiers.
fn alias_identifier(name: &str) -> Result<String> {
    if matches!(name, "self" | "Self" | "super" | "crate" | "_") {
        return Err(constraint("legacy alias cannot be a Rust identifier"));
    }
    if jmax_schema_aot::RUST_KEYWORDS.contains(&name) {
        // validate_symbol rejects keywords, so validate the identifier grammar
        // independently before adding the raw-identifier prefix.
        let mut bytes = name.bytes();
        if !bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(constraint("invalid legacy alias identifier"));
        }
        return Ok(format!("r#{name}"));
    }
    jmax_schema_aot::validate_symbol(name)?;
    Ok(String::from(name))
}

/// Follow owned edges only. References, including aliases and recursive nominal
/// references, have been checked by build_registry and must not be expanded.
struct Catalog<'a> {
    types: BTreeMap<&'a str, &'a EndpointType>,
    nominals: BTreeMap<&'a str, &'a EndpointType>,
    object: bool,
}

impl<'a> Catalog<'a> {
    fn collect(model: &'a EndpointModel) -> Result<Self> {
        let mut catalog = Self {
            types: BTreeMap::new(), nominals: BTreeMap::new(), object: false,
        };
        for definition in &model.definitions {
            if catalog.types.insert(&definition.name, &definition.ty).is_some() {
                return Err(constraint("duplicate declared model type name"));
            }
        }
        for definition in &model.definitions { catalog.visit(&definition.ty, 0)?; }
        for service in &model.services {
            for endpoint in &service.endpoints {
                for field in endpoint.params.iter().chain(&endpoint.response) {
                    catalog.visit(&field.ty, 0)?;
                }
                if let Some(ty) = &endpoint.stream_response { catalog.visit(ty, 0)?; }
            }
        }
        for header in &model.headers { catalog.visit(&header.ty, 0)?; }
        Ok(catalog)
    }

    fn visit(&mut self, ty: &'a EndpointType, depth: usize) -> Result<()> {
        check_depth(depth)?;
        match ty {
            EndpointType::Struct { name, fields } => {
                self.intern(name, ty)?;
                for field in fields { self.visit(&field.ty, depth + 1)?; }
            }
            EndpointType::Enum { name, variants } => {
                validate_variants(variants)?;
                self.intern(name, ty)?;
            }
            EndpointType::Vec(inner) | EndpointType::Optional(inner) => {
                self.visit(inner, depth + 1)?;
            }
            EndpointType::Object => self.object = true,
            _ => {}
        }
        Ok(())
    }

    fn intern(&mut self, name: &'a str, ty: &'a EndpointType) -> Result<()> {
        if self.nominals.insert(name, ty).is_some() {
            return Err(constraint("duplicate nominal model name"));
        }
        if let Some(existing) = self.types.get(name) {
            if !core::ptr::eq(*existing, ty) {
                return Err(constraint("nominal model name collides with a definition"));
            }
        } else {
            self.types.insert(name, ty);
        }
        Ok(())
    }

    fn validate_names(&self, options: &CodegenOptions) -> Result<()> {
        let mut names = BTreeMap::new();
        if self.object {
            reserve_name(&mut names, String::from("JsonObject"))?;
            reserve_name(&mut names, String::from("JsonValue"))?;
        }
        for name in self.types.keys() {
            let canonical = pascal(name)?;
            reserve_name(&mut names, canonical.clone())?;
            if options.emit_views && self.nominals.contains_key(name) {
                reserve_name(&mut names, format!("{canonical}View"))?;
            }
        }
        Ok(())
    }
}

fn reserve_name(names: &mut BTreeMap<String, ()>, name: String) -> Result<()> {
    if names.insert(name, ()).is_some() {
        return Err(constraint("colliding generated compatibility names"));
    }
    Ok(())
}

fn validate_variants(variants: &[EnumVariantSpec]) -> Result<()> {
    if variants.is_empty() {
        return Err(constraint("empty enum has no numeric JSON representation"));
    }
    let mut names = BTreeMap::new();
    let mut values = BTreeMap::new();
    for variant in variants {
        reserve_name(&mut names, pascal(&variant.name)?)?;
        if values.insert(variant.value, ()).is_some() {
            return Err(constraint("duplicate generated enum discriminant"));
        }
    }
    Ok(())
}

fn emit_aliases(out: &mut String, catalog: &Catalog<'_>, options: &CodegenOptions) -> Result<()> {
    let mut aliases = BTreeMap::new();
    // Re-exports may introduce constructors in the value namespace as well as
    // names in the type namespace. Reserve the metadata names conservatively.
    for name in ["LEGACY_VERSION", "MODEL_VERSION", "DEFINITION_VARIANTS"] {
        reserve_name(&mut aliases, String::from(name))?;
    }
    for name in catalog.types.keys() {
        let canonical = pascal(name)?;
        let alias = alias_identifier(name)?;
        reserve_name(&mut aliases, String::from(*name))?;
        out.push_str(&format!("#[allow(non_camel_case_types)]\npub use super::models::{canonical} as {alias};\n"));
        if options.emit_views && catalog.nominals.contains_key(name) {
            let original_view = format!("{name}View");
            let alias_view = alias_identifier(&original_view)?;
            reserve_name(&mut aliases, original_view)?;
            out.push_str(&format!("#[allow(non_camel_case_types)]\npub use super::models::{canonical}View as {alias_view};\n"));
        }
    }
    Ok(())
}

/// Generate compatibility metadata and unambiguous legacy model re-exports.
///
/// The legacy exporter validates whether the requested projection is lossless;
/// its errors retain their original ABI context. Native mode still validates
/// the complete model and all canonical names, even with metadata disabled.
/// Source is returned in memory, with no added derives or trait implementations.
pub fn generate_compat(model: &EndpointModel, options: &CodegenOptions) -> Result<GeneratedArtifact> {
    validate_options(options)?;
    let legacy_version = match options.compatibility.mode {
        CompatibilityMode::Native => {
            let _registry = jmax_endpoint_model::registry::build_registry(model)?;
            "::core::option::Option::None"
        }
        CompatibilityMode::LegacyV1 => {
            let _document = jmax_endpoint_model::legacy::export_legacy(model, LegacyVersion::V1)?;
            "::core::option::Option::Some(1u16)"
        }
        CompatibilityMode::LegacyV2 => {
            let _document = jmax_endpoint_model::legacy::export_legacy(model, LegacyVersion::V2)?;
            "::core::option::Option::Some(2u16)"
        }
    };
    let catalog = Catalog::collect(model)?;
    catalog.validate_names(options)?;

    let mut out = String::from(GENERATED_NOTICE);
    out.push_str(&format!("pub const LEGACY_VERSION: ::core::option::Option<u16> = {legacy_version};\n"));
    out.push_str(&format!("pub const MODEL_VERSION: (u32, u32, u32) = ({}u32, {}u32, {}u32);\n",
        model.version.major, model.version.minor, model.version.patch));
    out.push_str(&format!("pub const DEFINITION_VARIANTS: {VARIANTS_TYPE} = &["));
    if options.compatibility.emit_definition_variants {
        let mut any = false;
        for (name, ty) in &catalog.nominals {
            if let EndpointType::Enum { variants, .. } = ty {
                if !any { out.push('\n'); any = true; }
                out.push_str(&format!("    ({}, &[\n", rust_string_literal(name)));
                for variant in variants {
                    out.push_str(&format!("        ({}, {}, {}),\n",
                        rust_string_literal(&variant.name), variant.value,
                        rust_string_literal(&variant.description)));
                }
                out.push_str("    ]),\n");
            }
        }
    }
    out.push_str("];\n");
    if options.compatibility.mode != CompatibilityMode::Native {
        emit_aliases(&mut out, &catalog, options)?;
    }
    Ok(GeneratedArtifact { path: format!("{}/compat.rs", options.module_name),
        contents: out, kind: ArtifactKind::Compatibility })
}
