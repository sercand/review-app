//! File-type and folder icons: the path → icon-name mapping and the
//! bundled SVGs from `material-icon-theme@5.11.1` (MIT, `assets/file-icons/`).

use eframe::egui;

/// The icon name for a file path. Matching is case-insensitive on the file
/// name; the first rule that matches wins.
pub fn file_icon_name(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    // 1. Whole names.
    match name.as_str() {
        "package.json" | "package-lock.json" => return "nodejs",
        "cargo.toml" | "cargo.lock" => return "rust",
        "dockerfile" => return "docker",
        "readme.md" => return "readme",
        ".gitignore" | ".gitattributes" => return "git",
        _ => {}
    }
    // 2. Test files.
    if name.ends_with(".test.ts") || name.ends_with(".spec.ts") {
        return "test-ts";
    }
    if name.ends_with(".test.tsx")
        || name.ends_with(".spec.tsx")
        || name.ends_with(".test.jsx")
        || name.ends_with(".spec.jsx")
    {
        return "test-jsx";
    }
    if name.ends_with(".test.js") || name.ends_with(".spec.js") {
        return "test-js";
    }
    // 3-5. Extensions; 6. anything else is `file`.
    let ext = match name.rfind('.') {
        Some(i) if i + 1 < name.len() => &name[i + 1..],
        _ => return "file",
    };
    match ext {
        // languages
        "ts" => "typescript",
        "tsx" => "react_ts",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "react",
        "rs" => "rust",
        "go" => "go",
        "py" => "python",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "cs" => "csharp",
        "cpp" | "cc" | "hpp" => "cpp",
        "c" | "h" => "c",
        "lua" => "lua",
        "svelte" => "svelte",
        "vue" => "vue",
        "proto" => "proto",
        "sql" => "database",
        "sh" | "bash" | "zsh" => "console",
        // markup and config
        "html" => "html",
        "css" => "css",
        "scss" | "sass" => "sass",
        "json" => "json",
        "yml" | "yaml" => "yaml",
        "toml" => "settings",
        "md" => "markdown",
        // binary and assets
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => "image",
        "svg" => "svg",
        "lock" => "lock",
        "ttf" | "otf" | "woff" | "woff2" => "font",
        "pdf" => "pdf",
        "zip" | "gz" | "tar" => "zip",
        _ => "file",
    }
}

/// The icon name for a folder. The last segment of a chain label
/// (`"apps/metadata"` → `metadata`) selects the icon; `open` picks the
/// `-open` variant.
pub fn folder_icon_name(name: &str, open: bool) -> &'static str {
    let last = name.rsplit('/').next().unwrap_or(name).to_lowercase();
    let (closed, open_name) = match last.as_str() {
        "src" => ("folder-src", "folder-src-open"),
        "lib" => ("folder-lib", "folder-lib-open"),
        "components" => ("folder-components", "folder-components-open"),
        "test" | "tests" | "__tests__" => ("folder-test", "folder-test-open"),
        "docs" => ("folder-docs", "folder-docs-open"),
        "public" => ("folder-public", "folder-public-open"),
        "scripts" => ("folder-scripts", "folder-scripts-open"),
        "config" => ("folder-config", "folder-config-open"),
        "images" | "img" | "assets" => ("folder-images", "folder-images-open"),
        ".github" => ("folder-github", "folder-github-open"),
        _ => ("folder", "folder-open"),
    };
    if open {
        open_name
    } else {
        closed
    }
}

/// Every name [`file_icon_name`] and [`folder_icon_name`] can return, open
/// variants included.
pub const ALL_ICON_NAMES: &[&str] = &[
    "nodejs",
    "rust",
    "docker",
    "readme",
    "git",
    "test-ts",
    "test-jsx",
    "test-js",
    "typescript",
    "react_ts",
    "javascript",
    "react",
    "go",
    "python",
    "java",
    "kotlin",
    "swift",
    "csharp",
    "cpp",
    "c",
    "lua",
    "svelte",
    "vue",
    "proto",
    "database",
    "console",
    "html",
    "css",
    "sass",
    "json",
    "yaml",
    "settings",
    "markdown",
    "image",
    "svg",
    "lock",
    "font",
    "pdf",
    "zip",
    "file",
    "folder",
    "folder-open",
    "folder-src",
    "folder-src-open",
    "folder-lib",
    "folder-lib-open",
    "folder-components",
    "folder-components-open",
    "folder-test",
    "folder-test-open",
    "folder-docs",
    "folder-docs-open",
    "folder-public",
    "folder-public-open",
    "folder-scripts",
    "folder-scripts-open",
    "folder-config",
    "folder-config-open",
    "folder-images",
    "folder-images-open",
    "folder-github",
    "folder-github-open",
];

/// The bundled source for an icon name, or `None` for an unknown name.
pub fn icon_source(name: &str) -> Option<egui::ImageSource<'static>> {
    let src = match name {
        "nodejs" => egui::include_image!("../../assets/file-icons/nodejs.svg"),
        "rust" => egui::include_image!("../../assets/file-icons/rust.svg"),
        "docker" => egui::include_image!("../../assets/file-icons/docker.svg"),
        "readme" => egui::include_image!("../../assets/file-icons/readme.svg"),
        "git" => egui::include_image!("../../assets/file-icons/git.svg"),
        "test-ts" => egui::include_image!("../../assets/file-icons/test-ts.svg"),
        "test-jsx" => egui::include_image!("../../assets/file-icons/test-jsx.svg"),
        "test-js" => egui::include_image!("../../assets/file-icons/test-js.svg"),
        "typescript" => egui::include_image!("../../assets/file-icons/typescript.svg"),
        "react_ts" => egui::include_image!("../../assets/file-icons/react_ts.svg"),
        "javascript" => egui::include_image!("../../assets/file-icons/javascript.svg"),
        "react" => egui::include_image!("../../assets/file-icons/react.svg"),
        "go" => egui::include_image!("../../assets/file-icons/go.svg"),
        "python" => egui::include_image!("../../assets/file-icons/python.svg"),
        "java" => egui::include_image!("../../assets/file-icons/java.svg"),
        "kotlin" => egui::include_image!("../../assets/file-icons/kotlin.svg"),
        "swift" => egui::include_image!("../../assets/file-icons/swift.svg"),
        "csharp" => egui::include_image!("../../assets/file-icons/csharp.svg"),
        "cpp" => egui::include_image!("../../assets/file-icons/cpp.svg"),
        "c" => egui::include_image!("../../assets/file-icons/c.svg"),
        "lua" => egui::include_image!("../../assets/file-icons/lua.svg"),
        "svelte" => egui::include_image!("../../assets/file-icons/svelte.svg"),
        "vue" => egui::include_image!("../../assets/file-icons/vue.svg"),
        "proto" => egui::include_image!("../../assets/file-icons/proto.svg"),
        "database" => egui::include_image!("../../assets/file-icons/database.svg"),
        "console" => egui::include_image!("../../assets/file-icons/console.svg"),
        "html" => egui::include_image!("../../assets/file-icons/html.svg"),
        "css" => egui::include_image!("../../assets/file-icons/css.svg"),
        "sass" => egui::include_image!("../../assets/file-icons/sass.svg"),
        "json" => egui::include_image!("../../assets/file-icons/json.svg"),
        "yaml" => egui::include_image!("../../assets/file-icons/yaml.svg"),
        "settings" => egui::include_image!("../../assets/file-icons/settings.svg"),
        "markdown" => egui::include_image!("../../assets/file-icons/markdown.svg"),
        "image" => egui::include_image!("../../assets/file-icons/image.svg"),
        "svg" => egui::include_image!("../../assets/file-icons/svg.svg"),
        "lock" => egui::include_image!("../../assets/file-icons/lock.svg"),
        "font" => egui::include_image!("../../assets/file-icons/font.svg"),
        "pdf" => egui::include_image!("../../assets/file-icons/pdf.svg"),
        "zip" => egui::include_image!("../../assets/file-icons/zip.svg"),
        "file" => egui::include_image!("../../assets/file-icons/file.svg"),
        "folder" => egui::include_image!("../../assets/file-icons/folder.svg"),
        "folder-open" => egui::include_image!("../../assets/file-icons/folder-open.svg"),
        "folder-src" => egui::include_image!("../../assets/file-icons/folder-src.svg"),
        "folder-src-open" => egui::include_image!("../../assets/file-icons/folder-src-open.svg"),
        "folder-lib" => egui::include_image!("../../assets/file-icons/folder-lib.svg"),
        "folder-lib-open" => egui::include_image!("../../assets/file-icons/folder-lib-open.svg"),
        "folder-components" => {
            egui::include_image!("../../assets/file-icons/folder-components.svg")
        }
        "folder-components-open" => {
            egui::include_image!("../../assets/file-icons/folder-components-open.svg")
        }
        "folder-test" => egui::include_image!("../../assets/file-icons/folder-test.svg"),
        "folder-test-open" => egui::include_image!("../../assets/file-icons/folder-test-open.svg"),
        "folder-docs" => egui::include_image!("../../assets/file-icons/folder-docs.svg"),
        "folder-docs-open" => egui::include_image!("../../assets/file-icons/folder-docs-open.svg"),
        "folder-public" => egui::include_image!("../../assets/file-icons/folder-public.svg"),
        "folder-public-open" => {
            egui::include_image!("../../assets/file-icons/folder-public-open.svg")
        }
        "folder-scripts" => egui::include_image!("../../assets/file-icons/folder-scripts.svg"),
        "folder-scripts-open" => {
            egui::include_image!("../../assets/file-icons/folder-scripts-open.svg")
        }
        "folder-config" => egui::include_image!("../../assets/file-icons/folder-config.svg"),
        "folder-config-open" => {
            egui::include_image!("../../assets/file-icons/folder-config-open.svg")
        }
        "folder-images" => egui::include_image!("../../assets/file-icons/folder-images.svg"),
        "folder-images-open" => {
            egui::include_image!("../../assets/file-icons/folder-images-open.svg")
        }
        "folder-github" => egui::include_image!("../../assets/file-icons/folder-github.svg"),
        "folder-github-open" => {
            egui::include_image!("../../assets/file-icons/folder-github-open.svg")
        }
        _ => return None,
    };
    Some(src)
}

/// Draws the 16×16 icon for a file path.
pub fn file_icon(ui: &mut egui::Ui, path: &str) {
    draw_icon(ui, file_icon_name(path));
}

/// Draws the 16×16 icon for a folder label.
pub fn folder_icon(ui: &mut egui::Ui, name: &str, open: bool) {
    draw_icon(ui, folder_icon_name(name, open));
}

fn draw_icon(ui: &mut egui::Ui, name: &str) {
    if let Some(src) = icon_source(name) {
        ui.add(egui::Image::new(src).fit_to_exact_size(egui::vec2(16.0, 16.0)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_map_to_icons() {
        for (path, icon) in [
            ("src/lib/data-tools/push.ts", "typescript"),
            ("a/node-pane.tsx", "react_ts"),
            ("a/node-kinds.test.ts", "test-ts"),
            ("a/x.spec.tsx", "test-jsx"),
            ("x.test.js", "test-js"),
            ("Cargo.toml", "rust"),
            ("web/package-lock.json", "nodejs"),
            ("README.md", "readme"),
            ("docs/guide.md", "markdown"),
            ("public/icons/push-badge.PNG", "image"),
            ("fonts/badge-glyphs.woff2", "font"),
            ("config.toml", "settings"),
            ("db/q.sql", "database"),
            ("Makefile", "file"),
            ("no_extension", "file"),
        ] {
            assert_eq!(file_icon_name(path), icon, "{path}");
        }
    }

    #[test]
    fn folders_map_to_icons() {
        assert_eq!(folder_icon_name("src", false), "folder-src");
        assert_eq!(folder_icon_name("apps/metadata", true), "folder-open");
        assert_eq!(folder_icon_name("__tests__", true), "folder-test-open");
        assert_eq!(
            folder_icon_name("components", true),
            "folder-components-open"
        );
    }

    #[test]
    fn every_icon_name_is_bundled() {
        for name in ALL_ICON_NAMES {
            assert!(
                icon_source(name).is_some(),
                "{name} has no include_image arm"
            );
            let path = format!(
                "{}/assets/file-icons/{name}.svg",
                env!("CARGO_MANIFEST_DIR")
            );
            assert!(std::path::Path::new(&path).exists(), "{path} missing");
        }
    }
}
