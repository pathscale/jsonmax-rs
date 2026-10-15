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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec, vec::Vec};
    use crate::{CompatibilityOptions, EndpointField, EndpointSchemaSpec, ErrorSource,
        ErrorSpec, Generation, HeaderSpec, ModelVersion, RoleSpec, ServiceSpec, TypeDefinition};

    const MODES: [CompatibilityMode; 3] = [CompatibilityMode::Native,
        CompatibilityMode::LegacyV1, CompatibilityMode::LegacyV2];

    fn options(mode: CompatibilityMode, variants: bool) -> CodegenOptions {
        CodegenOptions { compatibility: CompatibilityOptions {
            mode, emit_definition_variants: variants,
        }, ..CodegenOptions::default() }
    }

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::new() }
    }

    fn definition(name: &str, ty: EndpointType) -> TypeDefinition {
        TypeDefinition { name: String::from(name), ty }
    }

    fn variant(name: &str, value: i32, description: &str) -> EnumVariantSpec {
        EnumVariantSpec { name: String::from(name), value, description: String::from(description) }
    }

    fn enumeration(name: &str) -> EndpointType {
        EndpointType::Enum { name: String::from(name), variants: vec![
            variant("High", i32::MAX, "最大"), variant("Low", i32::MIN, "最小"),
            variant("Zero", 0, ""),
        ] }
    }

    fn record(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn endpoint() -> EndpointSchemaSpec {
        EndpointSchemaSpec { code: u32::MAX, name: String::from("get"),
            service: String::from("api"), roles: vec![],
            params: vec![field("id", EndpointType::UInt32)],
            response: vec![field("text", EndpointType::String)], stream_response: None,
            description: String::from("説明\n"), }
    }

    fn simple_model() -> EndpointModel {
        EndpointModel { version: ModelVersion::new(u32::MAX, 0, 42),
            definitions: vec![definition("State", enumeration("State"))],
            services: vec![ServiceSpec { id: u16::MAX, name: String::from("api"),
                endpoints: vec![endpoint()] }], ..EndpointModel::default() }
    }

    fn assert_error(error: crate::Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn empty_artifacts_have_exact_metadata_in_every_flag_combination() {
        for (mode, version) in MODES.into_iter().zip([
            "::core::option::Option::None", "::core::option::Option::Some(1u16)",
            "::core::option::Option::Some(2u16)",
        ]) {
            for variants in [false, true] {
                for views in [false, true] {
                    for tables in [false, true] {
                        let mut config = options(mode, variants);
                        config.emit_views = views;
                        config.emit_schema_tables = tables;
                        let artifact = generate_compat(&EndpointModel::default(), &config).unwrap();
                        assert_eq!(artifact.path, "generated/compat.rs");
                        assert_eq!(artifact.kind, ArtifactKind::Compatibility);
                        assert_eq!(artifact.contents, format!("{GENERATED_NOTICE}pub const LEGACY_VERSION: ::core::option::Option<u16> = {version};\npub const MODEL_VERSION: (u32, u32, u32) = (0u32, 0u32, 0u32);\npub const DEFINITION_VARIANTS: {VARIANTS_TYPE} = &[];\n"));
                        let mut generation = Generation::default();
                        generation.insert(artifact).unwrap();
                        assert_eq!(generation.artifacts().len(), 1);
                    }
                }
            }
        }
    }

    #[test]
    fn all_model_version_components_preserve_zero_and_u32_boundaries() {
        for version in [ModelVersion::new(0, 0, 0), ModelVersion::new(1, 2, 3),
            ModelVersion::new(u32::MAX, u32::MAX, u32::MAX)] {
            let model = EndpointModel { version, ..EndpointModel::default() };
            for mode in MODES {
                let source = generate_compat(&model, &options(mode, true)).unwrap().contents;
                assert!(source.contains(&format!("= ({}u32, {}u32, {}u32);\n",
                    version.major, version.minor, version.patch)));
            }
        }
    }

    #[test]
    fn numeric_enum_vector_and_utf8_descriptions_are_exact_and_ordered() {
        let model = simple_model();
        let source = generate_compat(&model, &options(CompatibilityMode::Native, true)).unwrap().contents;
        assert!(source.contains("    (\"State\", &[\n        (\"High\", 2147483647, \"最大\"),\n        (\"Low\", -2147483648, \"最小\"),\n        (\"Zero\", 0, \"\"),\n    ]),\n"));
        assert!(!source.contains("pub use"));
        assert!(source.ends_with("];\n"));
        assert!(!source.ends_with("\n\n"));
        assert_eq!(generate_compat(&model, &options(CompatibilityMode::Native, true)).unwrap().contents, source);
    }

    #[test]
    fn every_owned_location_is_collected_once_and_sorted_by_original_name() {
        let mut model = simple_model();
        model.definitions = vec![definition("Zulu", enumeration("Zulu")),
            definition("Container", record("Container", vec![field("inner",
                EndpointType::Optional(Box::new(EndpointType::Vec(Box::new(enumeration("Nested"))))))])),
            definition("Alias", EndpointType::EnumRef { name: String::from("Zulu"), prefixed_name: true })];
        let endpoint = &mut model.services[0].endpoints[0];
        endpoint.params.push(field("state", enumeration("RequestState")));
        endpoint.response.push(field("state", enumeration("ResponseState")));
        endpoint.stream_response = Some(enumeration("StreamState"));
        model.headers.push(HeaderSpec { name: String::from("x-state"), ty: enumeration("HeaderState"),
            required: true, description: String::new() });
        let before = model.clone();
        let source = generate_compat(&model, &options(CompatibilityMode::LegacyV2, true)).unwrap().contents;
        let mut config = options(CompatibilityMode::LegacyV2, true);
        config.emit_schema_tables = false;
        let models = crate::models::generate_models(&model, &config).unwrap().contents;
        let ordered = ["HeaderState", "Nested", "RequestState", "ResponseState", "StreamState", "Zulu"];
        let mut previous = 0;
        for name in ordered {
            let needle = format!("    (\"{name}\", &[\n");
            assert_eq!(source.matches(needle.as_str()).count(), 1);
            let position = source.find(needle.as_str()).unwrap();
            assert!(position > previous);
            previous = position;
            assert!(source.contains(&format!("pub use super::models::{name} as {name};")));
            assert!(models.contains(&format!("pub enum {name} {{")));
            assert!(models.contains(&format!("pub struct {name}View<'a>")));
        }
        assert!(!source.contains("    (\"Alias\","));
        assert!(!source.contains("    (\"Container\","));
        assert_eq!(model, before);
    }

    #[test]
    fn bytewise_original_name_sorting_is_independent_of_declaration_order() {
        let mut model = EndpointModel { definitions: vec![
            definition("alpha_state", enumeration("alpha_state")),
            definition("_beta_state", enumeration("_beta_state")),
            definition("ZState", enumeration("ZState")),
        ], ..EndpointModel::default() };
        let config = options(CompatibilityMode::LegacyV2, true);
        let first = generate_compat(&model, &config).unwrap();
        let source = &first.contents;
        let upper = source.find("    (\"ZState\", &[").unwrap();
        let underscore = source.find("    (\"_beta_state\", &[").unwrap();
        let lower = source.find("    (\"alpha_state\", &[").unwrap();
        assert!(upper < underscore && underscore < lower);
        let upper = source.find("pub use super::models::ZState as ZState;").unwrap();
        let underscore = source.find("pub use super::models::BetaState as _beta_state;").unwrap();
        let lower = source.find("pub use super::models::AlphaState as alpha_state;").unwrap();
        assert!(upper < underscore && underscore < lower);
        model.definitions.reverse();
        assert_eq!(generate_compat(&model, &config).unwrap(), first);
    }

    #[test]
    fn literal_escaping_keeps_arbitrary_descriptions_inside_data() {
        let text = "quote\" slash\\ lf\n cr\r tab\t nul\0 é🦀\u{2028}\u{2029}";
        let ty = EndpointType::Enum { name: String::from("State"),
            variants: vec![variant("Ready", -7, text)] };
        let model = EndpointModel { definitions: vec![definition("State", ty)], ..EndpointModel::default() };
        let source = generate_compat(&model, &options(CompatibilityMode::Native, true)).unwrap().contents;
        assert!(source.contains(&format!("(\"Ready\", -7, {}),", rust_string_literal(text))));
        assert!(!source.contains(text));
        assert!(!source.contains('\r'));
        assert!(!source.contains('\0'));
        assert!(!source.contains("#[derive"));
    }

    #[test]
    fn disabled_variants_emit_an_explicit_empty_slice_and_no_bridge() {
        for mode in MODES {
            let config = options(mode, false);
            let model = simple_model();
            let source = generate_compat(&model, &config).unwrap().contents;
            assert!(source.contains(&format!("pub const DEFINITION_VARIANTS: {VARIANTS_TYPE} = &[];\n")));
            assert!(!source.contains("最大"));
            assert!(!source.contains("JMAX_DEFINITION_VARIANTS"));
            let models = crate::models::generate_models(&model, &config).unwrap().contents;
            assert!(!models.contains("JMAX_DEFINITION_VARIANTS"));
        }
    }

    #[test]
    fn legacy_reexports_preserve_original_names_keywords_and_owner_views() {
        let model = EndpointModel { definitions: vec![
            definition("http_state", enumeration("http_state")),
            definition("type", EndpointType::UInt32),
            definition("state_alias", EndpointType::EnumRef {
                name: String::from("http_state"), prefixed_name: true }),
        ], ..EndpointModel::default() };
        for mode in [CompatibilityMode::LegacyV1, CompatibilityMode::LegacyV2] {
            let source = generate_compat(&model, &options(mode, true)).unwrap().contents;
            for line in ["pub use super::models::HttpState as http_state;",
                "pub use super::models::HttpStateView as http_stateView;",
                "pub use super::models::Type as r#type;",
                "pub use super::models::StateAlias as state_alias;"] {
                assert!(source.contains(line));
            }
            assert!(!source.contains("StateAliasView"));
            assert!(!source.contains("TypeView"));
            let mut config = options(mode, true);
            config.emit_views = false;
            assert!(!generate_compat(&model, &config).unwrap().contents.contains("View as"));
        }
    }

    #[test]
    fn legacy_v1_rejects_each_unrepresentable_feature_without_loss() {
        let mut cases = Vec::new();
        let mut errors = simple_model();
        errors.errors.push(ErrorSpec { code: 0, name: String::from("Failure"), message: String::from("失敗") });
        cases.push(errors);
        let mut headers = simple_model();
        headers.headers.push(HeaderSpec { name: String::from("x-id"), ty: EndpointType::String,
            required: true, description: String::new() });
        cases.push(headers);
        for description in ["description", " ", "\t\n", "説明"] {
            let mut model = simple_model();
            model.roles.push(RoleSpec { name: String::from("reader"), includes: vec![],
                description: String::from(description) });
            cases.push(model);
        }
        let mut inheritance = simple_model();
        inheritance.roles = vec![RoleSpec { name: String::from("reader"), includes: vec![], description: String::new() },
            RoleSpec { name: String::from("writer"), includes: vec![String::from("reader")], description: String::new() }];
        cases.push(inheritance);
        for model in cases {
            let before = model.clone();
            let expected = jmax_endpoint_model::legacy::export_legacy(&model, LegacyVersion::V1).unwrap_err();
            assert_error(expected.clone(), ErrorCode::UnsupportedSchemaFeature);
            for variants in [false, true] {
                assert_eq!(generate_compat(&model, &options(CompatibilityMode::LegacyV1, variants)).unwrap_err(), expected);
                for mode in [CompatibilityMode::Native, CompatibilityMode::LegacyV2] {
                    assert!(generate_compat(&model, &options(mode, variants)).is_ok());
                }
            }
            assert_eq!(model, before);
        }
    }

    #[test]
    fn legacy_v1_accepts_plain_roles_and_preserves_descriptions_that_it_can_represent() {
        let mut model = simple_model();
        model.roles.push(RoleSpec { name: String::from("reader"), includes: vec![], description: String::new() });
        model.services[0].endpoints[0].roles.push(String::from("reader"));
        let before = model.clone();
        let artifact = generate_compat(&model, &options(CompatibilityMode::LegacyV1, true)).unwrap();
        assert!(artifact.contents.contains("Some(1u16)"));
        assert!(artifact.contents.contains("最大"));
        assert_eq!(model, before);
    }

    #[test]
    fn malformed_models_propagate_registry_errors_in_every_mode() {
        let mut cases = Vec::new();
        let mut duplicate = simple_model();
        let mut other = endpoint();
        other.name = String::from("other");
        duplicate.services[0].endpoints.push(other);
        cases.push(duplicate);
        let mut ownership = simple_model();
        ownership.services[0].endpoints[0].service = String::from("other");
        cases.push(ownership);
        let mut roles = simple_model();
        roles.services[0].endpoints[0].roles.push(String::from("missing"));
        cases.push(roles);
        cases.push(EndpointModel { definitions: vec![definition("Alias", EndpointType::StructRef(String::from("Missing")))],
            ..EndpointModel::default() });
        cases.push(EndpointModel { definitions: vec![definition("A", EndpointType::StructRef(String::from("B"))),
            definition("B", EndpointType::StructRef(String::from("A")))], ..EndpointModel::default() });
        let mut fields = simple_model();
        fields.services[0].endpoints[0].params.push(field("id", EndpointType::String));
        cases.push(fields);
        let mut discriminants = simple_model();
        discriminants.definitions[0].ty = EndpointType::Enum { name: String::from("State"),
            variants: vec![variant("A", 1, ""), variant("B", 1, "")] };
        cases.push(discriminants);
        for model in cases {
            let before = model.clone();
            let expected = jmax_endpoint_model::registry::build_registry(&model).unwrap_err();
            for mode in MODES {
                for variants in [false, true] {
                    assert_eq!(generate_compat(&model, &options(mode, variants)).unwrap_err(), expected);
                }
            }
            assert_eq!(model, before);
        }
    }

    #[test]
    fn invalid_options_precede_model_processing_and_interpolation() {
        let mut model = simple_model();
        model.services[0].name.clear();
        let mut cases = Vec::new();
        for module in ["", "../escape", "CON", "mod", "é", "a-b"] {
            cases.push(CodegenOptions { module_name: String::from(module), ..CodegenOptions::default() });
        }
        for path in ["", "::abi; injected()", "::self::abi", "crate::abi::<T>", "::abi\n"] {
            cases.push(CodegenOptions { abi_path: String::from(path), ..CodegenOptions::default() });
            cases.push(CodegenOptions { aot_path: String::from(path), ..CodegenOptions::default() });
            cases.push(CodegenOptions { derive_path: String::from(path), ..CodegenOptions::default() });
        }
        for config in cases {
            let expected = validate_options(&config).unwrap_err();
            assert_eq!(generate_compat(&model, &config).unwrap_err(), expected);
        }
    }

    #[test]
    fn canonical_type_variant_support_and_view_collisions_are_rejected() {
        let cases = [
            EndpointModel { definitions: vec![definition("foo_bar", EndpointType::String),
                definition("FooBar", EndpointType::UInt32)], ..EndpointModel::default() },
            EndpointModel { definitions: vec![definition("State", enumeration("State")),
                definition("StateView", EndpointType::String)], ..EndpointModel::default() },
            EndpointModel { definitions: vec![definition("JsonObject", EndpointType::String),
                definition("Value", EndpointType::Object)], ..EndpointModel::default() },
            EndpointModel { definitions: vec![definition("Empty", EndpointType::Enum {
                name: String::from("Empty"), variants: vec![] })], ..EndpointModel::default() },
            EndpointModel { definitions: vec![definition("State", EndpointType::Enum {
                name: String::from("State"), variants: vec![variant("foo_bar", 0, ""), variant("FooBar", 1, "")] })],
                ..EndpointModel::default() },
        ];
        for model in cases {
            jmax_endpoint_model::registry::build_registry(&model).unwrap();
            for mode in MODES {
                for variants in [false, true] {
                    assert_error(generate_compat(&model, &options(mode, variants)).unwrap_err(), ErrorCode::ConstraintViolation);
                }
            }
        }
    }

    #[test]
    fn legacy_metadata_namespace_and_unescapable_names_cannot_be_reexported() {
        for name in ["LEGACY_VERSION", "MODEL_VERSION", "DEFINITION_VARIANTS", "self", "super", "crate"] {
            let model = EndpointModel { definitions: vec![definition(name, EndpointType::UInt32)],
                ..EndpointModel::default() };
            for mode in [CompatibilityMode::LegacyV1, CompatibilityMode::LegacyV2] {
                assert_error(generate_compat(&model, &options(mode, false)).unwrap_err(), ErrorCode::ConstraintViolation);
            }
            if name != "self" {
                assert!(generate_compat(&model, &options(CompatibilityMode::Native, false)).is_ok());
            }
        }
        for name in ["", "_", "Self", "self", "super", "crate", "bad-name", "a::b", "a;panic!()", "é"] {
            assert!(alias_identifier(name).is_err());
        }
        assert_eq!(alias_identifier("type").unwrap(), "r#type");
        assert_eq!(alias_identifier("HTTPState").unwrap(), "HTTPState");
    }

    #[test]
    fn recursive_nominals_and_reference_aliases_are_finite_and_have_no_extra_enum_rows() {
        let model = EndpointModel { definitions: vec![
            definition("Node", record("Node", vec![field("next",
                EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("Node")))))])),
            definition("NodeAlias", EndpointType::StructTable { struct_ref: String::from("Node") }),
            definition("State", enumeration("State")),
            definition("StateAlias", EndpointType::EnumRef { name: String::from("State"), prefixed_name: true }),
        ], ..EndpointModel::default() };
        for mode in MODES {
            let source = generate_compat(&model, &options(mode, true)).unwrap().contents;
            assert_eq!(source.matches("    (\"State\", &[").count(), 1);
            assert!(!source.contains("    (\"StateAlias\","));
            assert!(!source.contains("    (\"Node\","));
        }
    }

    #[test]
    fn depth_and_internal_collision_boundaries_return_structured_errors() {
        for depth in [0, 1, 127] { assert!(check_depth(depth).is_ok()); }
        for depth in [128, 129, usize::MAX] {
            assert_error(check_depth(depth).unwrap_err(), ErrorCode::DepthLimitExceeded);
        }
        let mut ty = EndpointType::UInt32;
        for _ in 0..128 { ty = EndpointType::Vec(Box::new(ty)); }
        let model = EndpointModel { definitions: vec![definition("Deep", ty)], ..EndpointModel::default() };
        for mode in MODES {
            assert_error(generate_compat(&model, &options(mode, false)).unwrap_err(), ErrorCode::DepthLimitExceeded);
        }
        let ty = enumeration("State");
        let mut catalog = Catalog { types: BTreeMap::new(), nominals: BTreeMap::new(), object: false };
        catalog.visit(&ty, 0).unwrap();
        assert_error(catalog.visit(&ty, 0).unwrap_err(), ErrorCode::ConstraintViolation);
        let other = enumeration("State");
        let mut catalog = Catalog { types: BTreeMap::new(), nominals: BTreeMap::new(), object: false };
        catalog.types.insert("State", &ty);
        assert_error(catalog.visit(&other, 0).unwrap_err(), ErrorCode::ConstraintViolation);
        let mut names = BTreeMap::new();
        reserve_name(&mut names, String::from("Name")).unwrap();
        assert_error(reserve_name(&mut names, String::from("Name")).unwrap_err(), ErrorCode::ConstraintViolation);
        assert_error(validate_variants(&[variant("A", 7, ""), variant("B", 7, "")]).unwrap_err(),
            ErrorCode::ConstraintViolation);
    }

    #[test]
    fn complete_generation_keeps_schema_and_payload_artifacts_identical_across_legacy_modes() {
        let model = simple_model();
        let before = model.clone();
        for variants in [false, true] {
            for views in [false, true] {
                for tables in [false, true] {
                    let mut config = options(CompatibilityMode::Native, variants);
                    config.module_name = String::from("client");
                    config.emit_views = views;
                    config.emit_schema_tables = tables;
                    config.abi_path = String::from("crate::abi");
                    config.aot_path = String::from("crate::aot");
                    config.derive_path = String::from("crate::derive_bridge");
                    let native = crate::generate(&model, &config).unwrap();
                    assert_eq!(native.artifacts().len(), 6);
                    let paths: Vec<_> = native.artifacts().iter().map(|item| item.path.as_str()).collect();
                    assert_eq!(paths, ["client/compat.rs", "client/endpoints.rs", "client/mod.rs",
                        "client/models.rs", "client/registries.rs", "client/schema_tables.rs"]);
                    let model_source = &native.artifacts().iter().find(|item| item.kind == ArtifactKind::Models).unwrap().contents;
                    let compat_source = &native.artifacts().iter().find(|item| item.kind == ArtifactKind::Compatibility).unwrap().contents;
                    let endpoint_source = &native.artifacts().iter().find(|item| item.kind == ArtifactKind::Endpoints).unwrap().contents;
                    let registry_source = &native.artifacts().iter().find(|item| item.kind == ArtifactKind::Registries).unwrap().contents;
                    let schema_source = &native.artifacts().iter().find(|item| item.kind == ArtifactKind::SchemaTables).unwrap().contents;
                    assert!(endpoint_source.contains("pub const CODE: u32 = 4294967295u32;"));
                    assert!(endpoint_source.contains("pub const METHOD: &'static str = \"api.get\";"));
                    assert!(registry_source.contains("EndpointEntry { code: 4294967295u32, method: \"api.get\""));
                    assert!(schema_source.contains(if tables {
                        "pub const SCHEMA_COUNT: usize = 2;"
                    } else {
                        "pub const SCHEMA_COUNT: usize = 0;"
                    }));
                    assert_eq!(model_source.contains("JMAX_DEFINITION_VARIANTS"), variants);
                    if variants {
                        for tuple in ["(\"High\", 2147483647, \"最大\"),", "(\"Low\", -2147483648, \"最小\"),", "(\"Zero\", 0, \"\"),"] {
                            assert!(model_source.contains(tuple));
                            assert!(compat_source.contains(tuple));
                        }
                    }
                    for mode in [CompatibilityMode::LegacyV1, CompatibilityMode::LegacyV2] {
                        config.compatibility.mode = mode;
                        let legacy = crate::generate(&model, &config).unwrap();
                        for (native_item, legacy_item) in native.artifacts().iter().zip(legacy.artifacts()) {
                            assert_eq!(native_item.path, legacy_item.path);
                            assert_eq!(native_item.kind, legacy_item.kind);
                            if native_item.kind != ArtifactKind::Compatibility {
                                assert_eq!(native_item, legacy_item);
                            }
                            assert!(legacy_item.contents.starts_with(GENERATED_NOTICE));
                            assert!(legacy_item.contents.ends_with('\n'));
                            assert!(!legacy_item.contents.ends_with("\n\n"));
                        }
                    }
                }
            }
        }
        assert_eq!(model, before);
    }

    #[test]
    fn prefixed_enum_references_do_not_change_canonical_metadata_or_numeric_encoding() {
        let mut model = simple_model();
        model.definitions.push(definition("Alias", EndpointType::EnumRef {
            name: String::from("State"), prefixed_name: false }));
        for mode in MODES {
            let config = options(mode, true);
            let first = generate_compat(&model, &config).unwrap();
            model.definitions[1].ty = EndpointType::EnumRef { name: String::from("State"), prefixed_name: true };
            assert_eq!(generate_compat(&model, &config).unwrap(), first);
            let source = crate::models::generate_models(&model, &config).unwrap().contents;
            assert!(source.contains("write_i64(i64::from(*self as i32), sink)"));
            model.definitions[1].ty = EndpointType::EnumRef { name: String::from("State"), prefixed_name: false };
        }
    }
}
