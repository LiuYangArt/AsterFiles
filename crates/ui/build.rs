use std::{collections::HashMap, path::Path};

use syn::{Ident, LitStr, Token, parse::Parse, punctuated::Punctuated};

struct UiTextResource {
    field: Ident,
    chinese: LitStr,
}

impl Parse for UiTextResource {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let field = input.parse()?;
        input.parse::<Token![:]>()?;
        let translations;
        syn::parenthesized!(translations in input);
        let chinese = translations.parse()?;
        translations.parse::<Token![,]>()?;
        let _: LitStr = translations.parse()?;
        if !translations.is_empty() {
            return Err(translations.error("expected exactly Chinese and English translations"));
        }
        Ok(Self { field, chinese })
    }
}

fn generate_ui_texts(resource_path: &Path, output_path: &Path) {
    println!("cargo:rerun-if-changed={}", resource_path.display());
    let source = std::fs::read_to_string(resource_path).expect("failed to read UI text resources");
    let resources = syn::parse_file(&source)
        .expect("invalid i18n Rust source")
        .items
        .into_iter()
        .find_map(|item| match item {
            syn::Item::Macro(item) if item.mac.path.is_ident("ui_texts") => Some(item.mac),
            _ => None,
        })
        .expect("missing ui_texts! resource declaration")
        .parse_body_with(Punctuated::<UiTextResource, Token![,]>::parse_terminated)
        .expect("invalid ui_texts! resource declaration");
    assert!(!resources.is_empty(), "UI text resources must not be empty");

    let mut structure = String::from("export struct UiTextValues {\n");
    let mut defaults = String::new();
    for resource in resources {
        let field = resource.field.to_string().replace('_', "-");
        structure.push_str(&format!("    {field}: string,\n"));
        defaults.push_str(&format!(
            "        {field}: {:?},\n",
            resource.chinese.value()
        ));
    }
    structure
        .push_str("}\nexport global UiTexts {\n    in-out property <UiTextValues> values: {\n");
    structure.push_str(&defaults);
    structure.push_str("    };\n}\n");
    std::fs::write(output_path, structure).expect("failed to write generated Slint UI text record");
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text_library =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("ui-texts.slint");
    generate_ui_texts(&root.join("src/i18n.rs"), &text_library);
    let emit_debug_info = std::env::var("PROFILE").is_ok_and(|profile| profile != "release");
    slint_build::compile_with_config(
        root.join("ui/app-window.slint"),
        slint_build::CompilerConfiguration::new()
            .with_debug_info(emit_debug_info)
            .with_library_paths(HashMap::from([("ui-texts".to_owned(), text_library)])),
    )
    .expect("failed to compile Slint UI");
}
