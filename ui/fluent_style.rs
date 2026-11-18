use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use i_slint_compiler::{
    fileaccess::{find_file, write_file_if_changed},
    lexer::lex,
    parser::SyntaxKind,
    source_path::SourcePath,
};

pub fn compile() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("fluent-system-colors");
    let builtin = SourcePath::new("builtin:/fluent");
    // Keep the pinned Fluent widgets and their resources, using desktop canvas
    // and card colors with Windows contrast-theme overrides.
    let mut pending = [
        "std-widgets.slint",
        "style-base.slint",
        "std-widgets-impl.slint",
    ]
    .map(|name| official_file(&builtin.join(name).unwrap()))
    .to_vec();
    let mut seen = HashSet::new();
    while let Some(file) = pending.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let SourcePath::Builtin(relative) = &file else {
            panic!("expected a built-in Slint resource, got {file}");
        };
        let relative = Path::new(relative.as_str());
        let destination = output.join(relative);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let bytes = file.read().unwrap();
        if relative.extension().is_some_and(|ext| ext == "slint") {
            let source = std::str::from_utf8(&bytes).unwrap();
            for token in lex(source) {
                if token.kind != SyntaxKind::StringLiteral {
                    continue;
                }
                let reference = token.text.trim_matches('"');
                if !Path::new(reference)
                    .extension()
                    .is_some_and(|ext| ext == "slint" || ext == "svg" || ext == "png")
                {
                    continue;
                }
                let candidate = file
                    .parent()
                    .join(reference)
                    .and_then(|path| find_file(&path))
                    .unwrap_or_else(|| official_file(&builtin.join(reference).unwrap()));
                pending.push(candidate);
            }
            let styling = relative == Path::new("fluent/styling.slint");
            let patched = contrast(source, styling);
            write_file_if_changed(&destination, patched.as_bytes()).unwrap();
        } else {
            write_file_if_changed(&destination, &bytes).unwrap();
        }
    }

    // The copied widget sources use Slint's internal types and widget interfaces.
    // Cargo runs this build script in its own process before the compiler starts.
    unsafe {
        env::set_var(
            "SLINT_ENABLE_EXPERIMENTAL_FEATURES",
            "internal-types,interfaces,shadowable,closures",
        )
    };
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_include_paths(vec![output.join("fluent")])
        .with_library_paths(HashMap::from([(
            "windows-contrast".into(),
            manifest.join("ui/system-colors.slint"),
        )]))
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/desktop.slint", config)
        .expect("compile the Fluent desktop");
}

fn official_file(path: &SourcePath) -> SourcePath {
    find_file(path).unwrap_or_else(|| panic!("missing pinned Slint resource {path}"))
}

fn contrast(source: &str, styling: bool) -> String {
    let mut code = String::with_capacity(source.len());
    code.push_str("import { WindowsContrast } from \"@windows-contrast\";\n");
    for line in source.lines() {
        let trimmed = line.trim_start();
        if styling
            && let Some((name, value)) = trimmed
                .strip_prefix("out property <brush> ")
                .and_then(|line| line.split_once(": "))
        {
            let role = contrast_role(name);
            let value = match name {
                "background" => "dark-color-scheme ? #202020 : #F3F3F3;",
                "alternate-background" => "dark-color-scheme ? #2B2B2B : #FFFFFF;",
                "control-border" => "dark-color-scheme ? #FFFFFF39 : #00000038;",
                "text-disabled" => "dark-color-scheme ? #FFFFFF88 : #00000080;",
                _ => value,
            };
            code.push_str(&format!("    out property <brush> {name}: WindowsContrast.colors.active ? {role} : {value}\n"));
        } else if let Some(radius) = trimmed
            .strip_prefix("border-radius: ")
            .and_then(|line| line.strip_suffix("px;"))
            .and_then(|line| line.parse::<u8>().ok())
        {
            code.push_str(&format!(
                "    border-radius: WindowsContrast.colors.active ? 0px : {radius}px;\n"
            ));
        } else {
            code.push_str(line);
            code.push('\n');
            if trimmed == "export component FocusBorder inherits Rectangle {" {
                code.push_str("    visible: WindowsContrast.keyboard-navigation;\n");
            }
        }
    }
    code
}

fn contrast_role(name: &str) -> &'static str {
    match name {
        "background"
        | "alternate-background"
        | "focus-stroke-inner"
        | "layer-on-mica-base-alt"
        | "layer-on-mica-base-alt-secondary"
        | "subtle"
        | "subtle-secondary"
        | "subtle-tertiary"
        | "sub-title-secondary"
        | "sub-title-tertiary" => "WindowsContrast.colors.window-color",
        "foreground"
        | "alternate-foreground"
        | "border"
        | "focus-stroke-outer"
        | "divider"
        | "card-stroke"
        | "control-background-stroke-flyout" => "WindowsContrast.colors.window-text",
        "control-background"
        | "control-secondary"
        | "control-tertiary"
        | "control-disabled"
        | "control-alt-secondary"
        | "control-alt-tertiary"
        | "control-alt-quaternary"
        | "control-solid"
        | "control-input-active"
        | "accent-disabled" => "WindowsContrast.colors.button-face",
        "control-foreground"
        | "control-strong-stroke"
        | "control-border"
        | "circle-border"
        | "text-control-border"
        | "text-secondary"
        | "text-tertiary" => "WindowsContrast.colors.button-text",
        "accent-background"
        | "selection-background"
        | "secondary-accent-background"
        | "tertiary-accent-background"
        | "accent-control-border" => "WindowsContrast.colors.highlight",
        "accent-foreground"
        | "selection-foreground"
        | "text-accent-foreground-secondary"
        | "state-secondary" => "WindowsContrast.colors.highlight-text",
        "text-disabled" | "text-accent-foreground-disabled" | "control-strong-stroke-disabled" => {
            "WindowsContrast.colors.gray-text"
        }
        "state" => "WindowsContrast.colors.window-text",
        "control-alt-disabled" | "shadow" => "transparent",
        _ => panic!("missing Windows contrast color for Fluent palette property {name}"),
    }
}
