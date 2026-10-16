//! Syntax highlighting with tree-sitter (ADR 0034, phase E3).
//!
//! A [`Syntax`] keeps a parse tree for one document. Edits from
//! [`Document::take_edits`](crate::Document::take_edits) update it
//! incrementally, and [`Syntax::highlights`] runs the language's highlight
//! query over a byte range only (the visible lines), so the cost follows the
//! screen, not the file.
//!
//! Above [`SYNC_PARSE_BYTES`] a parse takes longer than a frame, so it runs
//! on a background thread: edits move the current tree at once (colours stay
//! in place while the new parse runs), and the new tree replaces it when it
//! arrives, with any edits made meanwhile replayed on it.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use ropey::Rope;
use tree_sitter::{
    InputEdit, Language, Node, Parser, Point, Query, QueryCursor, StreamingIterator, TextProvider,
    Tree,
};

use crate::change::ByteEdit;

/// Documents larger than this are not parsed by tree-sitter (they stay plain
/// text). The parse runs in the background above [`SYNC_PARSE_BYTES`], so the
/// limit is memory: a tree takes 25–35 times the file's size (about 250 MB
/// for 8 MB of Rust on an M4, where the first parse takes 0.5 s and a
/// keystroke's reparse 65 ms).
pub const MAX_HIGHLIGHT_BYTES: usize = 8 << 20;

/// Documents up to this size are reparsed on the caller's thread: a
/// keystroke's reparse grows with the file (about 4 ms per MB of Rust,
/// tree-sitter re-walking the top-level items after the edit), and stays
/// within a frame here. Windows needs a smaller boundary: reparsing 512 KiB
/// exceeded the 16 ms frame budget on real hardware. Larger files still get
/// full highlighting, with parsing moved off the UI thread.
pub const SYNC_PARSE_BYTES: usize = if cfg!(windows) { 256 << 10 } else { 512 << 10 };

/// Sublime syntaxes (the fallback) parse line by line from checkpoints on the
/// caller's thread; a jump to the end of the file parses everything before
/// it once, so they keep a lower limit.
pub const MAX_FALLBACK_BYTES: usize = 1 << 20;

static PARSE_WAKER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Called from background threads (a finished parse, large-file indexing
/// progress) so the UI can draw again (the app wakes its event loop). Set
/// once; later calls are ignored.
pub fn set_parse_waker(wake: impl Fn() + Send + Sync + 'static) {
    let _ = PARSE_WAKER.set(Box::new(wake));
}

pub(crate) fn wake_background() {
    if let Some(wake) = PARSE_WAKER.get() {
        wake();
    }
}

/// What a highlighted span is, mapped to a colour by the editor's palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Highlight {
    Keyword,
    String,
    Escape,
    Comment,
    Number,
    Constant,
    Function,
    Type,
    Property,
    Variable,
    Tag,
    Attribute,
    Operator,
    Punctuation,
    Heading,
    Link,
}

impl Highlight {
    /// A highlight-query capture name (`keyword.control`, `string.special`,
    /// `function.method`…) by its most significant part.
    pub fn from_capture(name: &str) -> Option<Highlight> {
        let head = name.split('.').next().unwrap_or(name);
        Some(match head {
            "keyword" | "include" | "conditional" | "repeat" | "exception" | "storageclass" => {
                Highlight::Keyword
            }
            "string" if name.contains("escape") || name.contains("special") => Highlight::Escape,
            "string" | "character" => Highlight::String,
            "escape" => Highlight::Escape,
            "comment" => Highlight::Comment,
            "number" | "float" | "boolean" => Highlight::Number,
            // Literals: numbers, and the built-in constants queries tag the
            // same way (`true`, `None`, Rust's integer literals).
            "constant" if name.contains("numeric") || name.contains("builtin") => Highlight::Number,
            "constant" => Highlight::Constant,
            "function" | "method" | "constructor" => Highlight::Function,
            "type" => Highlight::Type,
            "property" | "field" => Highlight::Property,
            "variable" if name.contains("builtin") || name.contains("parameter") => {
                Highlight::Variable
            }
            "label" | "module" | "namespace" => Highlight::Type,
            "tag" => Highlight::Tag,
            "attribute" => Highlight::Attribute,
            "operator" => Highlight::Operator,
            "punctuation" => Highlight::Punctuation,
            "text" | "markup" if name.contains("title") || name.contains("heading") => {
                Highlight::Heading
            }
            "text" | "markup" if name.contains("uri") || name.contains("link") => Highlight::Link,
            "text" | "markup" if name.contains("literal") || name.contains("raw") => {
                Highlight::String
            }
            _ => return None,
        })
    }
}

/// A built-in language: how files are recognised, its grammar and query.
struct LangDef {
    name: &'static str,
    extensions: &'static [&'static str],
    file_names: &'static [&'static str],
    language: fn() -> Language,
    query: fn() -> String,
}

fn q(parts: &[&str]) -> String {
    parts.join("\n")
}

static LANGS: &[LangDef] = &[
    LangDef {
        name: "Rust",
        extensions: &["rs"],
        file_names: &[],
        language: || tree_sitter_rust::LANGUAGE.into(),
        query: || tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "JavaScript",
        extensions: &["js", "mjs", "cjs", "jsx"],
        file_names: &[],
        language: || tree_sitter_javascript::LANGUAGE.into(),
        query: || {
            q(&[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            ])
        },
    },
    LangDef {
        name: "TypeScript",
        extensions: &["ts", "mts", "cts"],
        file_names: &[],
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        query: || {
            q(&[
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY,
            ])
        },
    },
    LangDef {
        name: "TSX",
        extensions: &["tsx"],
        file_names: &[],
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        query: || {
            q(&[
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            ])
        },
    },
    LangDef {
        name: "Python",
        extensions: &["py", "pyi", "pyw"],
        file_names: &[],
        language: || tree_sitter_python::LANGUAGE.into(),
        query: || tree_sitter_python::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Go",
        extensions: &["go"],
        file_names: &[],
        language: || tree_sitter_go::LANGUAGE.into(),
        query: || tree_sitter_go::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "C",
        extensions: &["c", "h"],
        file_names: &[],
        language: || tree_sitter_c::LANGUAGE.into(),
        query: || tree_sitter_c::HIGHLIGHT_QUERY.into(),
    },
    LangDef {
        name: "C++",
        extensions: &["cc", "cpp", "cxx", "hpp", "hh", "hxx", "ipp"],
        file_names: &[],
        language: || tree_sitter_cpp::LANGUAGE.into(),
        query: || {
            q(&[
                tree_sitter_cpp::HIGHLIGHT_QUERY,
                tree_sitter_c::HIGHLIGHT_QUERY,
            ])
        },
    },
    LangDef {
        name: "Java",
        extensions: &["java"],
        file_names: &[],
        language: || tree_sitter_java::LANGUAGE.into(),
        query: || tree_sitter_java::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Shell",
        extensions: &["sh", "bash", "ksh", "env"],
        file_names: &[
            ".bashrc",
            ".bash_profile",
            ".profile",
            "PKGBUILD",
            ".env",
            ".env.local",
            ".env.example",
            ".env.development",
            ".env.production",
        ],
        language: || tree_sitter_bash::LANGUAGE.into(),
        query: || tree_sitter_bash::HIGHLIGHT_QUERY.into(),
    },
    LangDef {
        name: "JSON",
        extensions: &["json", "jsonc", "json5", "webmanifest"],
        file_names: &[".prettierrc", ".babelrc"],
        language: || tree_sitter_json::LANGUAGE.into(),
        query: || tree_sitter_json::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "TOML",
        extensions: &["toml"],
        file_names: &["Cargo.lock", "Pipfile"],
        language: || tree_sitter_toml_ng::LANGUAGE.into(),
        query: || tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "YAML",
        extensions: &["yaml", "yml"],
        file_names: &[],
        language: || tree_sitter_yaml::LANGUAGE.into(),
        query: || tree_sitter_yaml::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "HTML",
        extensions: &["html", "htm", "xhtml"],
        file_names: &[],
        language: || tree_sitter_html::LANGUAGE.into(),
        query: || tree_sitter_html::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "CSS",
        extensions: &["css"],
        file_names: &[],
        language: || tree_sitter_css::LANGUAGE.into(),
        query: || tree_sitter_css::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Markdown",
        extensions: &["md", "markdown", "mdx"],
        file_names: &[],
        language: || tree_sitter_md::LANGUAGE.into(),
        query: || tree_sitter_md::HIGHLIGHT_QUERY_BLOCK.into(),
    },
    LangDef {
        name: "Lua",
        extensions: &["lua"],
        file_names: &[],
        language: || tree_sitter_lua::LANGUAGE.into(),
        query: || tree_sitter_lua::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Ruby",
        extensions: &["rb", "rake", "gemspec"],
        file_names: &["Gemfile", "Rakefile"],
        language: || tree_sitter_ruby::LANGUAGE.into(),
        query: || tree_sitter_ruby::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Swift",
        extensions: &["swift"],
        file_names: &[],
        language: || tree_sitter_swift::LANGUAGE.into(),
        query: || tree_sitter_swift::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Makefile",
        extensions: &["mk", "mak"],
        file_names: &["Makefile", "makefile", "GNUmakefile"],
        language: || tree_sitter_make::LANGUAGE.into(),
        query: || tree_sitter_make::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "C#",
        extensions: &["cs", "csx"],
        file_names: &[],
        language: || tree_sitter_c_sharp::LANGUAGE.into(),
        query: || tree_sitter_c_sharp::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "PHP",
        extensions: &["php", "phtml"],
        file_names: &[],
        language: || tree_sitter_php::LANGUAGE_PHP.into(),
        query: || tree_sitter_php::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Kotlin",
        extensions: &["kt", "kts"],
        file_names: &[],
        language: || tree_sitter_kotlin_sg::LANGUAGE.into(),
        query: || tree_sitter_kotlin_sg::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Scala",
        extensions: &["scala", "sc", "sbt"],
        file_names: &[],
        language: || tree_sitter_scala::LANGUAGE.into(),
        query: || tree_sitter_scala::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Dart",
        extensions: &["dart"],
        file_names: &[],
        language: || tree_sitter_dart::LANGUAGE.into(),
        query: || tree_sitter_dart::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Objective-C",
        extensions: &["m", "mm"],
        file_names: &[],
        language: || tree_sitter_objc::LANGUAGE.into(),
        query: || {
            q(&[
                tree_sitter_objc::HIGHLIGHTS_QUERY,
                tree_sitter_c::HIGHLIGHT_QUERY,
            ])
        },
    },
    LangDef {
        name: "Perl",
        extensions: &["pl", "pm", "t"],
        file_names: &[],
        language: || arborium_perl::language().into(),
        query: || arborium_perl::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "R",
        extensions: &["r", "R"],
        file_names: &[],
        language: || tree_sitter_r::LANGUAGE.into(),
        query: || tree_sitter_r::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "PowerShell",
        extensions: &["ps1", "psm1", "psd1"],
        file_names: &[],
        language: || tree_sitter_powershell::LANGUAGE.into(),
        query: || tree_sitter_powershell::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Haskell",
        extensions: &["hs", "lhs"],
        file_names: &[],
        language: || tree_sitter_haskell::LANGUAGE.into(),
        query: || tree_sitter_haskell::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Elixir",
        extensions: &["ex", "exs"],
        file_names: &["mix.lock"],
        language: || tree_sitter_elixir::LANGUAGE.into(),
        query: || tree_sitter_elixir::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "HEEx",
        extensions: &["heex"],
        file_names: &[],
        language: || tree_sitter_heex::LANGUAGE.into(),
        query: || tree_sitter_heex::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Erlang",
        extensions: &["erl", "hrl"],
        file_names: &["rebar.config"],
        language: || tree_sitter_erlang::LANGUAGE.into(),
        query: || tree_sitter_erlang::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Zig",
        extensions: &["zig", "zon"],
        file_names: &[],
        language: || tree_sitter_zig::LANGUAGE.into(),
        query: || tree_sitter_zig::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Groovy",
        extensions: &["groovy", "gradle", "gvy"],
        file_names: &["Jenkinsfile"],
        language: || dekobon_tree_sitter_groovy::LANGUAGE.into(),
        query: || dekobon_tree_sitter_groovy::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Clojure",
        extensions: &["clj", "cljs", "cljc", "edn"],
        file_names: &[],
        language: || tree_sitter_clojure_orchard::LANGUAGE.into(),
        query: || tree_sitter_clojure_orchard::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Elm",
        extensions: &["elm"],
        file_names: &[],
        language: || tree_sitter_elm::LANGUAGE.into(),
        query: || tree_sitter_elm::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Gleam",
        extensions: &["gleam"],
        file_names: &[],
        language: || tree_sitter_gleam::LANGUAGE.into(),
        query: || tree_sitter_gleam::HIGHLIGHT_QUERY.into(),
    },
    LangDef {
        name: "Solidity",
        extensions: &["sol"],
        file_names: &[],
        language: || tree_sitter_solidity::LANGUAGE.into(),
        query: || tree_sitter_solidity::HIGHLIGHT_QUERY.into(),
    },
    LangDef {
        name: "Visual Basic",
        extensions: &["vb", "vbs", "bas"],
        file_names: &[],
        language: || arborium_vb::language().into(),
        query: || arborium_vb::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "MATLAB",
        extensions: &["matlab"],
        file_names: &[],
        language: || arborium_matlab::language().into(),
        query: || arborium_matlab::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Scheme",
        extensions: &["scm", "ss"],
        file_names: &[],
        language: || tree_sitter_scheme::LANGUAGE.into(),
        query: || tree_sitter_scheme::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Racket",
        extensions: &["rkt"],
        file_names: &[],
        language: || tree_sitter_racket::LANGUAGE.into(),
        query: || tree_sitter_racket::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Common Lisp",
        extensions: &["lisp", "lsp", "cl", "asd"],
        file_names: &[],
        language: || arborium_commonlisp::language().into(),
        query: || arborium_commonlisp::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Assembly",
        extensions: &["asm", "s", "S"],
        file_names: &[],
        language: || tree_sitter_asm::LANGUAGE.into(),
        query: || tree_sitter_asm::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Ada",
        extensions: &["ada", "adb", "ads"],
        file_names: &[],
        language: || arborium_ada::language().into(),
        query: || arborium_ada::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Vue",
        extensions: &["vue"],
        file_names: &[],
        language: || arborium_vue::language().into(),
        query: || q(&[&arborium_vue::HIGHLIGHTS_QUERY]),
    },
    LangDef {
        name: "Svelte",
        extensions: &["svelte"],
        file_names: &[],
        language: || tree_sitter_svelte_ng::LANGUAGE.into(),
        query: || {
            q(&[
                tree_sitter_svelte_ng::HIGHLIGHTS_QUERY,
                tree_sitter_html::HIGHLIGHTS_QUERY,
            ])
        },
    },
    LangDef {
        name: "SCSS",
        extensions: &["scss", "sass"],
        file_names: &[],
        language: || arborium_scss::language().into(),
        query: || q(&[&arborium_scss::HIGHLIGHTS_QUERY]),
    },
    LangDef {
        name: "LESS",
        extensions: &["less"],
        file_names: &[],
        language: || tree_sitter_less::language(),
        query: || tree_sitter_less::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "GraphQL",
        extensions: &["graphql", "gql"],
        file_names: &[],
        language: || arborium_graphql::language().into(),
        query: || arborium_graphql::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "XML",
        extensions: &[
            "xml", "svg", "xsd", "xsl", "xslt", "plist", "csproj", "fsproj", "vcxproj", "xaml",
            "rss", "atom",
        ],
        file_names: &[],
        language: || tree_sitter_xml::LANGUAGE_XML.into(),
        query: || tree_sitter_xml::XML_HIGHLIGHT_QUERY.into(),
    },
    LangDef {
        name: "Embedded Template",
        extensions: &["erb", "ejs"],
        file_names: &[],
        language: || tree_sitter_embedded_template::LANGUAGE.into(),
        query: || tree_sitter_embedded_template::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Jinja",
        extensions: &["j2", "jinja", "jinja2"],
        file_names: &[],
        language: || arborium_jinja2::language().into(),
        query: || arborium_jinja2::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Regex",
        extensions: &["regex"],
        file_names: &[],
        language: || tree_sitter_regex::LANGUAGE.into(),
        query: || tree_sitter_regex::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Dockerfile",
        extensions: &["dockerfile", "containerfile"],
        file_names: &["Dockerfile", "Containerfile"],
        language: || tree_sitter_containerfile::LANGUAGE.into(),
        query: || tree_sitter_containerfile::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "HCL",
        extensions: &["hcl", "tf", "tfvars", "nomad"],
        file_names: &[],
        language: || arborium_hcl::language().into(),
        query: || arborium_hcl::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Nix",
        extensions: &["nix"],
        file_names: &[],
        language: || tree_sitter_nix::LANGUAGE.into(),
        query: || tree_sitter_nix::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "INI",
        extensions: &["ini", "cfg", "conf", "editorconfig", "desktop", "service"],
        file_names: &[".editorconfig", ".gitconfig", ".npmrc"],
        language: || tree_sitter_ini::LANGUAGE.into(),
        query: || tree_sitter_ini::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "CMake",
        extensions: &["cmake"],
        file_names: &["CMakeLists.txt"],
        language: || tree_sitter_cmake::LANGUAGE.into(),
        query: || tree_sitter_cmake::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Meson",
        extensions: &["meson"],
        file_names: &["meson.build", "meson_options.txt"],
        language: || arborium_meson::language().into(),
        query: || arborium_meson::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Protobuf",
        extensions: &["proto"],
        file_names: &[],
        language: || arborium_proto::language().into(),
        query: || arborium_proto::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Thrift",
        extensions: &["thrift"],
        file_names: &[],
        language: || arborium_thrift::language().into(),
        query: || arborium_thrift::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "SQL",
        extensions: &["sql", "psql", "mysql"],
        file_names: &[],
        language: || tree_sitter_sequel::LANGUAGE.into(),
        query: || tree_sitter_sequel::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Diff",
        extensions: &["diff", "patch"],
        file_names: &[],
        language: || tree_sitter_diff::LANGUAGE.into(),
        query: || tree_sitter_diff::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Git Attributes",
        extensions: &[],
        file_names: &[".gitattributes"],
        language: || arborium_gitattributes::language().into(),
        query: || arborium_gitattributes::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Properties",
        extensions: &["properties"],
        file_names: &[],
        language: || tree_sitter_properties::LANGUAGE.into(),
        query: || tree_sitter_properties::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "SSH Config",
        extensions: &[],
        file_names: &["ssh_config", "sshd_config"],
        language: || arborium_ssh_config::language().into(),
        query: || arborium_ssh_config::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Just",
        extensions: &["just"],
        file_names: &["justfile", "Justfile", ".justfile"],
        language: || arborium_just::language().into(),
        query: || arborium_just::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Starlark",
        extensions: &["bzl", "star", "bazel"],
        file_names: &[
            "BUILD",
            "BUILD.bazel",
            "WORKSPACE",
            "MODULE.bazel",
            "Tiltfile",
        ],
        language: || tree_sitter_starlark::LANGUAGE.into(),
        query: || tree_sitter_starlark::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Fish",
        extensions: &["fish"],
        file_names: &[],
        language: || tree_sitter_fish::language(),
        query: || tree_sitter_fish::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Zsh",
        extensions: &["zsh"],
        file_names: &[".zshrc", ".zprofile", ".zshenv", ".zlogin"],
        language: || arborium_zsh::language().into(),
        query: || arborium_zsh::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Batch",
        extensions: &["bat", "cmd"],
        file_names: &[],
        language: || arborium_batch::language().into(),
        query: || arborium_batch::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Vim Script",
        extensions: &["vim"],
        file_names: &[".vimrc", "_vimrc"],
        language: || tree_sitter_vim::language(),
        query: || tree_sitter_vim::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "jq",
        extensions: &["jq"],
        file_names: &[],
        language: || arborium_jq::language().into(),
        query: || arborium_jq::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Bicep",
        extensions: &["bicep"],
        file_names: &[],
        language: || tree_sitter_bicep::LANGUAGE.into(),
        query: || tree_sitter_bicep::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Requirements",
        extensions: &[],
        file_names: &["requirements.txt", "constraints.txt"],
        language: || tree_sitter_requirements::LANGUAGE.into(),
        query: || tree_sitter_requirements::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "GLSL",
        extensions: &["glsl", "vert", "frag", "geom", "comp", "tesc", "tese"],
        file_names: &[],
        language: || tree_sitter_glsl::LANGUAGE_GLSL.into(),
        query: || tree_sitter_glsl::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "AsciiDoc",
        extensions: &["adoc", "asciidoc"],
        file_names: &[],
        language: || arborium_asciidoc::language().into(),
        query: || arborium_asciidoc::HIGHLIGHTS_QUERY.into(),
    },
    LangDef {
        name: "Typst",
        extensions: &["typ"],
        file_names: &[],
        language: || arborium_typst::language().into(),
        query: || arborium_typst::HIGHLIGHTS_QUERY.into(),
    },
];

/// The built-in language for a file, by name, then extension, then a `#!`
/// interpreter on the first line.
fn lang_for(path: &Path, first_line: &str) -> Option<&'static LangDef> {
    let name = path.file_name()?.to_str()?;
    if let Some(def) = LANGS.iter().find(|l| l.file_names.contains(&name)) {
        return Some(def);
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if let Some(def) = LANGS.iter().find(|l| l.extensions.contains(&ext.as_str())) {
            return Some(def);
        }
    }
    let shebang = first_line.strip_prefix("#!")?;
    let interpreter = shebang
        .split_whitespace()
        .flat_map(|w| w.rsplit('/').next())
        .find(|w| *w != "env")?;
    let ext = match interpreter.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.') {
        "sh" | "bash" | "dash" | "ksh" => "sh",
        "zsh" => "zsh",
        "fish" => "fish",
        "perl" => "pl",
        "php" => "php",
        "python" => "py",
        "node" | "deno" | "bun" => "js",
        "ruby" => "rb",
        "lua" => "lua",
        _ => return None,
    };
    LANGS.iter().find(|l| l.extensions.contains(&ext))
}

/// A compiled query and what each of its captures paints. Queries are built
/// once per language and shared by every document in it.
struct Compiled {
    query: Query,
    kinds: Vec<Option<Highlight>>,
}

/// Compiled queries by language name; `None` when a query failed to build.
type QueryCache = HashMap<&'static str, Option<Arc<Compiled>>>;

fn compiled(def: &'static LangDef) -> Option<Arc<Compiled>> {
    static CACHE: OnceLock<Mutex<QueryCache>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    cache
        .entry(def.name)
        .or_insert_with(|| {
            let query = Query::new(&(def.language)(), &(def.query)()).ok()?;
            let kinds = query
                .capture_names()
                .iter()
                .map(|n| Highlight::from_capture(n))
                .collect();
            Some(Arc::new(Compiled { query, kinds }))
        })
        .clone()
}

/// Rope text for tree-sitter predicates (`#match?`, `#eq?`).
struct RopeText<'a>(&'a Rope);

impl<'a> TextProvider<&'a [u8]> for RopeText<'a> {
    type I = Box<dyn Iterator<Item = &'a [u8]> + 'a>;

    fn text(&mut self, node: Node) -> Self::I {
        let range = node.byte_range();
        let end = range.end.min(self.0.len_bytes());
        let start = range.start.min(end);
        Box::new(self.0.byte_slice(start..end).chunks().map(str::as_bytes))
    }
}

/// The parse tree of one document in a built-in tree-sitter language.
struct TreeSyntax {
    name: &'static str,
    language: fn() -> Language,
    parser: Parser,
    tree: Option<Tree>,
    compiled: Arc<Compiled>,
    /// Edits were applied to the tree since it was last parsed.
    stale: bool,
    /// Parses off the caller's thread, once the document outgrew
    /// [`SYNC_PARSE_BYTES`].
    worker: Option<Worker>,
}

/// A text snapshot to parse, with the tree to reuse (edited to match it).
struct Job {
    rope: Rope,
    tree: Option<Tree>,
}

/// The background parse of one document. One job is in flight at a time;
/// edits made meanwhile wait in `since` and are replayed on its result.
struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Option<Tree>>,
    in_flight: bool,
    since: Vec<ByteEdit>,
    /// The text as of the latest edit (a rope clone shares its nodes).
    rope: Rope,
}

impl Worker {
    fn spawn(language: fn() -> Language, rope: Rope) -> Option<Worker> {
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (results, done) = mpsc::channel();
        std::thread::Builder::new()
            .name("mtty-parse".into())
            .spawn(move || {
                let mut parser = Parser::new();
                if parser.set_language(&language()).is_err() {
                    return;
                }
                // Ends when the document's Syntax is dropped.
                while let Ok(job) = inbox.recv() {
                    let tree = parse_rope(&mut parser, &job.rope, job.tree.as_ref());
                    if results.send(tree).is_err() {
                        return;
                    }
                    if let Some(wake) = PARSE_WAKER.get() {
                        wake();
                    }
                }
            })
            .ok()?;
        Some(Worker {
            jobs,
            done,
            in_flight: false,
            since: Vec::new(),
            rope,
        })
    }

    fn send(&mut self, tree: Option<Tree>) {
        let job = Job {
            rope: self.rope.clone(),
            tree,
        };
        self.in_flight = self.jobs.send(job).is_ok();
    }
}

fn parse_rope(parser: &mut Parser, rope: &Rope, old: Option<&Tree>) -> Option<Tree> {
    let len = rope.len_bytes();
    let mut read = |byte: usize, _: Point| -> &[u8] {
        if byte >= len {
            return &[];
        }
        let (chunk, start, _, _) = rope.chunk_at_byte(byte);
        &chunk.as_bytes()[byte - start..]
    };
    parser.parse_with_options(&mut read, old, None)
}

fn input_edit(e: &ByteEdit) -> InputEdit {
    InputEdit {
        start_byte: e.start_byte,
        old_end_byte: e.old_end_byte,
        new_end_byte: e.new_end_byte,
        start_position: Point::new(e.start.0, e.start.1),
        old_end_position: Point::new(e.old_end.0, e.old_end.1),
        new_end_position: Point::new(e.new_end.0, e.new_end.1),
    }
}

impl TreeSyntax {
    fn for_file(path: &Path, rope: &Rope, first: &str) -> Option<TreeSyntax> {
        let def = lang_for(path, first)?;
        let compiled = compiled(def)?;
        let mut parser = Parser::new();
        parser.set_language(&(def.language)()).ok()?;
        let mut syntax = TreeSyntax {
            name: def.name,
            language: def.language,
            parser,
            tree: None,
            compiled,
            stale: true,
            worker: None,
        };
        syntax.update(rope, &[]);
        Some(syntax)
    }

    /// The language's display name ("Rust", "TypeScript"…).
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Bring the tree up to date with `rope` after `edits` (from
    /// `Document::take_edits`): incremental when there is a tree.
    pub fn update(&mut self, rope: &Rope, edits: &[ByteEdit]) {
        if rope.len_bytes() > MAX_HIGHLIGHT_BYTES {
            // Too big to parse: drop the tree (and the worker with it).
            self.tree = None;
            self.worker = None;
            self.stale = true;
            return;
        }
        if let Some(tree) = &mut self.tree {
            for e in edits {
                tree.edit(&input_edit(e));
            }
        }
        if self.worker.is_none() && rope.len_bytes() > SYNC_PARSE_BYTES {
            self.worker = Worker::spawn(self.language, rope.clone());
        }
        if let Some(worker) = &mut self.worker {
            worker.rope = rope.clone();
            if worker.in_flight {
                worker.since.extend_from_slice(edits);
            } else if !edits.is_empty() || self.stale || self.tree.is_none() {
                worker.send(self.tree.clone());
                self.stale = false;
            }
            return;
        }
        if self.tree.is_some() && edits.is_empty() && !self.stale {
            return;
        }
        self.tree = parse_rope(&mut self.parser, rope, self.tree.as_ref());
        self.stale = false;
    }

    /// Take a finished background parse: replay the edits made since its
    /// job was sent, adopt the tree, and send the next job if there were
    /// any. True when the tree changed.
    fn poll(&mut self) -> bool {
        let Some(worker) = &mut self.worker else {
            return false;
        };
        let Ok(tree) = worker.done.try_recv() else {
            return false;
        };
        worker.in_flight = false;
        let Some(mut tree) = tree else {
            return false;
        };
        for e in &worker.since {
            tree.edit(&input_edit(e));
        }
        let more = !std::mem::take(&mut worker.since).is_empty();
        self.tree = Some(tree);
        if more {
            worker.send(self.tree.clone());
        }
        true
    }

    /// A background parse is running (or queued behind edits).
    fn parsing(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| w.in_flight)
    }

    /// Highlighted byte ranges within `range`, sorted and not overlapping.
    /// Where captures nest, the innermost wins; for the same span, the
    /// query's earlier pattern wins (the order highlight queries assume).
    pub fn highlights(&self, rope: &Rope, range: Range<usize>) -> Vec<(Range<usize>, Highlight)> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut found: Vec<(usize, usize, usize, Highlight)> = Vec::new();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range.clone());
        let mut captures = cursor.captures(&self.compiled.query, tree.root_node(), RopeText(rope));
        while let Some((m, i)) = captures.next() {
            let capture = m.captures[*i];
            let Some(kind) = self.compiled.kinds[capture.index as usize] else {
                continue;
            };
            let r = capture.node.byte_range();
            let (start, end) = (r.start.max(range.start), r.end.min(range.end));
            if start < end {
                found.push((start, end, m.pattern_index, kind));
            }
        }
        // Paint wide spans first, then narrower ones over them; on equal
        // spans the earliest pattern paints last and so wins.
        found.sort_by(|a, b| {
            (a.0, std::cmp::Reverse(a.1), std::cmp::Reverse(a.2)).cmp(&(
                b.0,
                std::cmp::Reverse(b.1),
                std::cmp::Reverse(b.2),
            ))
        });
        let mut painted: Vec<Option<Highlight>> = vec![None; range.end - range.start];
        for (start, end, _, kind) in found {
            for slot in &mut painted[start - range.start..end - range.start] {
                *slot = Some(kind);
            }
        }
        let mut out: Vec<(Range<usize>, Highlight)> = Vec::new();
        for (i, slot) in painted.into_iter().enumerate() {
            let at = range.start + i;
            match (slot, out.last_mut()) {
                (Some(k), Some((r, last))) if *last == k && r.end == at => r.end = at + 1,
                (Some(k), _) => out.push((at..at + 1, k)),
                (None, _) => {}
            }
        }
        out
    }

    /// Symbols for the outline, in source order: definition-like named nodes,
    /// each labelled by its `name` field (or first line when it has none).
    /// Best effort when a background parse is still running.
    pub fn outline(&self, rope: &Rope) -> Vec<OutlineSymbol> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut stack = vec![(tree.root_node(), 0usize)];
        while let Some((node, depth)) = stack.pop() {
            let kind = outline_kind(node.kind());
            let child_depth = if kind.is_some() { depth + 1 } else { depth };
            let mut children = Vec::new();
            let mut child = node.child(0);
            while let Some(c) = child {
                children.push(c);
                child = c.next_sibling();
            }
            for c in children.into_iter().rev() {
                stack.push((c, child_depth));
            }
            let Some(kind) = kind else { continue };
            out.push(OutlineSymbol {
                name: outline_name(node, rope),
                kind,
                line: rope.byte_to_line(node.start_byte()),
                depth,
            });
        }
        out
    }

    /// Foldable line ranges `(first, last)`, `last > first` (0-based): from
    /// the syntax tree when there is one (named nodes spanning more than one
    /// line, outermost per start line), else from indentation. `(first, last)`
    /// means folding `first` hides `first+1..=last`.
    pub fn folds(&self, rope: &Rope) -> Vec<(usize, usize)> {
        let Some(tree) = &self.tree else {
            return indent_folds(rope);
        };
        let last_line = crate::motion::last_line(rope);
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Start at the root's children: the root spans the whole file.
        let root = tree.root_node();
        let mut stack: Vec<Node> = Vec::new();
        let mut child = root.child(0);
        while let Some(c) = child {
            stack.push(c);
            child = c.next_sibling();
        }
        while let Some(node) = stack.pop() {
            // Push children first, in reverse, so the walk stays pre-order.
            let mut children = Vec::new();
            let mut c = node.child(0);
            while let Some(x) = c {
                children.push(x);
                c = x.next_sibling();
            }
            for x in children.into_iter().rev() {
                stack.push(x);
            }
            if !node.is_named() || node.is_error() {
                continue;
            }
            let (start_byte, end_byte) = (node.start_byte(), node.end_byte());
            if end_byte <= start_byte {
                continue;
            }
            let start = rope.byte_to_line(start_byte);
            let end = rope.byte_to_line(end_byte - 1).min(last_line);
            if end <= start || !seen.insert(start) {
                continue;
            }
            out.push((start, end));
        }
        out.sort_by_key(|(s, _)| *s);
        out
    }
}

/// Foldable ranges from indentation alone, for files without a tree: a line
/// is a fold start when the lines after it are more indented, up to the last
/// one that is. Blank lines inside a block do not end it.
fn indent_folds(rope: &Rope) -> Vec<(usize, usize)> {
    let last = crate::motion::last_line(rope);
    let indent_of = |line: usize| -> Option<usize> {
        let text: String = rope
            .line(line)
            .chars()
            .take_while(|c| *c != '\n' && *c != '\r')
            .collect();
        if text.trim().is_empty() {
            return None;
        }
        Some(text.chars().take_while(|c| *c == ' ' || *c == '\t').count())
    };
    let mut out = Vec::new();
    let mut line = 0;
    while line < last {
        let Some(indent) = indent_of(line) else {
            line += 1;
            continue;
        };
        let mut end = line;
        let mut j = line + 1;
        while j <= last {
            match indent_of(j) {
                Some(i) if i > indent => {
                    end = j;
                    j += 1;
                }
                None => j += 1,
                Some(_) => break,
            }
        }
        if end > line {
            out.push((line, end));
            line = end + 1;
        } else {
            line += 1;
        }
    }
    out
}

/// One entry in a file's outline (⌘R), taken from the syntax tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineSymbol {
    /// The node's `name` field, or its first line when it has none (`impl`).
    pub name: String,
    /// A coarse kind: `function`, `class`, `struct`, `enum`, `interface`,
    /// `impl`, `module`, `const`, `static`, `type`, `var`, `macro`.
    pub kind: &'static str,
    /// 0-based line the symbol starts on.
    pub line: usize,
    /// How many outline entries enclose it (for indentation in the picker).
    pub depth: usize,
}

/// The outline kind of a tree-sitter node, or `None` when the node is not an
/// outline entry. Definition-like nodes only, so a file's whole contents are
/// not listed.
fn outline_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "generator_function_declaration"
        | "method_definition"
        | "method_declaration" => "function",
        "class_definition" | "class_declaration" | "abstract_class_declaration" => "class",
        "struct_item" | "struct_specifier" => "struct",
        "enum_item" | "enum_declaration" | "enum_specifier" => "enum",
        "trait_item" | "interface_declaration" => "interface",
        "impl_item" => "impl",
        "mod_item" | "namespace_declaration" | "internal_module" | "module" => "module",
        "const_item" | "const_spec" => "const",
        "static_item" => "static",
        "type_item"
        | "type_alias_declaration"
        | "type_spec"
        | "type_definition"
        | "union_item"
        | "union_specifier" => "type",
        "var_spec" => "var",
        "macro_definition" => "macro",
        _ => return None,
    })
}

/// A symbol's label: its `name` field, else its first line with a trailing
/// `{` trimmed (an `impl` block or another unnamed declaration).
fn outline_name(node: Node, rope: &Rope) -> String {
    if let Some(name) = node.child_by_field_name("name") {
        let text = rope.byte_slice(name.byte_range()).to_string();
        let text = text.trim();
        if !text.is_empty() {
            return text.to_string();
        }
    }
    let first: String = rope
        .byte_slice(node.byte_range())
        .chars()
        .take_while(|c| *c != '\n')
        .collect();
    let first = first.trim().trim_end_matches('{').trim();
    let mut label: String = first.chars().take(80).collect();
    if first.chars().count() > 80 {
        label.push('…');
    }
    label
}

enum Engine {
    Tree(TreeSyntax),
    Sublime(crate::fallback::Fallback),
}

/// Highlighting for one document: a tree-sitter grammar when one of the
/// built-in languages claims the file, else a Sublime syntax from syntect's
/// default set (see [`crate::fallback`]).
pub struct Syntax {
    engine: Engine,
}

impl Syntax {
    /// Highlighting for `path` (its first line for a `#!`), when any engine
    /// knows the language.
    pub fn for_file(path: &Path, rope: &Rope) -> Option<Syntax> {
        let first: String = rope.lines().next().map(String::from).unwrap_or_default();
        let engine = match TreeSyntax::for_file(path, rope, &first) {
            Some(tree) => Engine::Tree(tree),
            None => Engine::Sublime(crate::fallback::Fallback::for_file(path, &first)?),
        };
        Some(Syntax { engine })
    }

    /// The language's display name ("Rust", "OCaml"…).
    pub fn name(&self) -> &'static str {
        match &self.engine {
            Engine::Tree(t) => t.name(),
            Engine::Sublime(f) => f.name(),
        }
    }

    /// Whether a tree-sitter grammar (not the line-based fallback) is used.
    pub fn is_tree_sitter(&self) -> bool {
        matches!(self.engine, Engine::Tree(_))
    }

    /// Catch up with `rope` after `edits` (from `Document::take_edits`).
    pub fn update(&mut self, rope: &Rope, edits: &[ByteEdit]) {
        match &mut self.engine {
            Engine::Tree(t) => t.update(rope, edits),
            Engine::Sublime(f) => {
                // Lines before the first edited one are unchanged in every
                // version the edits pass through.
                if let Some(row) = edits.iter().map(|e| e.start.0).min() {
                    f.invalidate_from(row);
                }
            }
        }
    }

    /// Take a finished background parse (call before drawing). True when
    /// the highlighting changed.
    pub fn poll(&mut self) -> bool {
        match &mut self.engine {
            Engine::Tree(t) => t.poll(),
            Engine::Sublime(_) => false,
        }
    }

    /// A background parse is running: what is drawn may lag the text.
    pub fn parsing(&self) -> bool {
        match &self.engine {
            Engine::Tree(t) => t.parsing(),
            Engine::Sublime(_) => false,
        }
    }

    /// Highlighted byte ranges within `range`, sorted and not overlapping.
    pub fn highlights(&self, rope: &Rope, range: Range<usize>) -> Vec<(Range<usize>, Highlight)> {
        match &self.engine {
            Engine::Tree(t) if rope.len_bytes() <= MAX_HIGHLIGHT_BYTES => t.highlights(rope, range),
            Engine::Sublime(f) if rope.len_bytes() <= MAX_FALLBACK_BYTES => {
                f.highlights(rope, range)
            }
            _ => Vec::new(),
        }
    }

    /// Symbols for the outline (⌘R). Tree-sitter only: the Sublime fallback
    /// has no tree, so it offers nothing.
    pub fn outline(&self, rope: &Rope) -> Vec<OutlineSymbol> {
        match &self.engine {
            Engine::Tree(t) if rope.len_bytes() <= MAX_HIGHLIGHT_BYTES => t.outline(rope),
            _ => Vec::new(),
        }
    }

    /// Foldable line ranges `(first, last)`: the syntax tree's when there is
    /// one, else indentation (the Sublime fallback has no tree).
    pub fn folds(&self, rope: &Rope) -> Vec<(usize, usize)> {
        match &self.engine {
            Engine::Tree(t) if rope.len_bytes() <= MAX_HIGHLIGHT_BYTES => {
                let folds = t.folds(rope);
                if folds.is_empty() {
                    indent_folds(rope)
                } else {
                    folds
                }
            }
            _ => indent_folds(rope),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Document, Selection};

    fn kinds_at(syntax: &Syntax, rope: &Rope, needle: &str) -> Vec<Highlight> {
        let text = rope.to_string();
        let at = text.find(needle).expect("needle in text");
        syntax
            .highlights(rope, 0..rope.len_bytes())
            .into_iter()
            .filter(|(r, _)| r.start <= at && at < r.end)
            .map(|(_, k)| k)
            .collect()
    }

    #[test]
    fn files_no_grammar_claims_fall_back_to_sublime_syntaxes() {
        let rope = Rope::from_str("let x = 1\n");
        let ml = Syntax::for_file(Path::new("a.ml"), &rope).unwrap();
        assert_eq!((ml.name(), ml.is_tree_sitter()), ("OCaml", false));
        let rs = Syntax::for_file(Path::new("a.rs"), &rope).unwrap();
        assert!(
            rs.is_tree_sitter(),
            "tree-sitter wins where both know the file"
        );
        assert!(Syntax::for_file(Path::new("a.txt"), &rope).is_none());
    }

    #[test]
    fn every_built_in_query_compiles() {
        for def in LANGS {
            assert!(compiled(def).is_some(), "{} query", def.name);
        }
    }

    #[test]
    fn languages_by_name_extension_and_shebang() {
        let name = |p: &str, first: &str| lang_for(Path::new(p), first).map(|d| d.name);
        assert_eq!(name("src/main.rs", ""), Some("Rust"));
        assert_eq!(name("App.TSX", ""), Some("TSX"));
        assert_eq!(name("Makefile", ""), Some("Makefile"));
        assert_eq!(name(".zshrc", ""), Some("Zsh"));
        assert_eq!(name(".env", ""), Some("Shell"));
        assert_eq!(name("Dockerfile", ""), Some("Dockerfile"));
        assert_eq!(name("CMakeLists.txt", ""), Some("CMake"));
        assert_eq!(name("main.tf", ""), Some("HCL"));
        assert_eq!(name("App.vue", ""), Some("Vue"));
        assert_eq!(name("Program.cs", ""), Some("C#"));
        assert_eq!(name("run", "#!/usr/bin/env zsh"), Some("Zsh"));
        assert_eq!(name("deploy", "#!/usr/bin/env bash"), Some("Shell"));
        assert_eq!(name("tool", "#!/usr/bin/python3.12"), Some("Python"));
        assert_eq!(name("notes.txt", ""), None);
    }

    #[test]
    fn extensions_and_file_names_are_not_claimed_twice() {
        let mut seen = std::collections::HashMap::new();
        for def in LANGS {
            for key in def.extensions.iter().chain(def.file_names) {
                if let Some(other) = seen.insert(key.to_ascii_lowercase(), def.name) {
                    assert_eq!(other, def.name, "{key} is claimed by two languages");
                }
            }
        }
        assert!(LANGS.len() >= 80, "{} built-in languages", LANGS.len());
    }

    #[test]
    fn a_few_of_the_added_languages_highlight() {
        for (path, text, needle, want) in [
            (
                "a.cs",
                "class A { int x = 1; }",
                "class",
                Highlight::Keyword,
            ),
            (
                "a.php",
                "<?php function f() { return 'x'; }",
                "'x'",
                Highlight::String,
            ),
            (
                "a.kt",
                "fun main() { val s = \"x\" }",
                "\"x\"",
                Highlight::String,
            ),
            (
                "q.sql",
                "SELECT id FROM users -- note",
                "-- note",
                Highlight::Comment,
            ),
            (
                "Dockerfile",
                "FROM alpine\nRUN echo hi",
                "FROM",
                Highlight::Keyword,
            ),
            (
                "main.tf",
                "resource \"a\" \"b\" { x = 1 }",
                "\"a\"",
                Highlight::String,
            ),
        ] {
            let rope = Rope::from_str(text);
            let syntax = Syntax::for_file(Path::new(path), &rope)
                .unwrap_or_else(|| panic!("{path}: no syntax"));
            let kinds = kinds_at(&syntax, &rope, needle);
            assert!(
                kinds.contains(&want),
                "{path}: {needle} is {kinds:?}, want {want:?}"
            );
        }
    }

    #[test]
    fn rust_keywords_strings_comments_and_functions() {
        let rope = Rope::from_str("// note\nfn main() { let s = \"hi\"; helper(1); }\n");
        let syntax = Syntax::for_file(Path::new("a.rs"), &rope).unwrap();
        assert_eq!(syntax.name(), "Rust");
        assert_eq!(
            kinds_at(&syntax, &rope, "// note"),
            vec![Highlight::Comment]
        );
        assert_eq!(kinds_at(&syntax, &rope, "fn"), vec![Highlight::Keyword]);
        assert_eq!(kinds_at(&syntax, &rope, "\"hi\""), vec![Highlight::String]);
        assert_eq!(
            kinds_at(&syntax, &rope, "helper"),
            vec![Highlight::Function]
        );
        assert_eq!(kinds_at(&syntax, &rope, "1)"), vec![Highlight::Number]);
    }

    #[test]
    fn typescript_uses_the_javascript_base_query() {
        let rope = Rope::from_str("const x: number = await load('a');\n");
        let syntax = Syntax::for_file(Path::new("a.ts"), &rope).unwrap();
        assert_eq!(kinds_at(&syntax, &rope, "const"), vec![Highlight::Keyword]);
        assert_eq!(kinds_at(&syntax, &rope, "'a'"), vec![Highlight::String]);
        assert_eq!(kinds_at(&syntax, &rope, "number"), vec![Highlight::Type]);
    }

    /// Poll until the background parse settles (or fail after 20 s).
    fn settle(syntax: &mut Syntax) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            syntax.poll();
            if !syntax.parsing() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "parse never settled");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn big_rust(bytes: usize) -> String {
        let unit = "pub fn compute(index: usize) -> usize {\n    index + 1 // note\n}\n\n";
        unit.repeat(bytes / unit.len() + 1)
    }

    #[test]
    fn large_files_parse_in_the_background_and_catch_up_with_edits() {
        static WAKES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        set_parse_waker(|| {
            WAKES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let mut doc = Document::from_text(&big_rust(SYNC_PARSE_BYTES * 2));
        let mut syntax = Syntax::for_file(Path::new("big.rs"), doc.rope()).unwrap();
        assert!(syntax.parsing(), "the first parse runs in the background");
        // Edit while that parse runs: the edits wait and are replayed.
        let middle = doc.rope().line_to_char(doc.rope().len_lines() / 2);
        doc.set_selection(Selection::cursor(middle));
        for line in ["// first marker\n", "fn added() {}\n", "// last marker\n"] {
            doc.type_text(line);
            let edits = doc.take_edits();
            syntax.update(doc.rope(), &edits);
        }
        settle(&mut syntax);
        let text = doc.rope().to_string();
        let kind_at = |needle: &str| {
            let at = text.find(needle).unwrap();
            syntax
                .highlights(doc.rope(), at..at + needle.len())
                .first()
                .map(|(_, k)| *k)
        };
        assert_eq!(kind_at("// first marker"), Some(Highlight::Comment));
        assert_eq!(kind_at("// last marker"), Some(Highlight::Comment));
        assert_eq!(kind_at("fn added"), Some(Highlight::Keyword));
        assert!(WAKES.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    #[test]
    fn small_files_parse_at_once_and_huge_ones_not_at_all() {
        let small = Rope::from_str("fn main() {}\n");
        let syntax = Syntax::for_file(Path::new("a.rs"), &small).unwrap();
        assert!(!syntax.parsing());
        assert!(!syntax.highlights(&small, 0..small.len_bytes()).is_empty());
        let huge = Rope::from_str(&big_rust(MAX_HIGHLIGHT_BYTES + 1));
        let mut syntax = Syntax::for_file(Path::new("huge.rs"), &huge).unwrap();
        assert!(!syntax.parsing(), "no background parse past the limit");
        assert!(!syntax.poll());
        assert!(syntax.highlights(&huge, 0..1000).is_empty());
    }

    #[test]
    fn incremental_edits_keep_the_tree_in_step() {
        let mut doc = Document::from_text("fn a() {}\n");
        let mut syntax = Syntax::for_file(Path::new("a.rs"), doc.rope()).unwrap();
        doc.set_selection(Selection::cursor(doc.rope().len_chars()));
        doc.type_text("// later");
        let edits = doc.take_edits();
        syntax.update(doc.rope(), &edits);
        assert_eq!(
            kinds_at(&syntax, doc.rope(), "// later"),
            vec![Highlight::Comment]
        );
        doc.undo();
        let edits = doc.take_edits();
        syntax.update(doc.rope(), &edits);
        let all = syntax.highlights(doc.rope(), 0..doc.rope().len_bytes());
        assert!(all.iter().all(|(r, _)| r.end <= doc.rope().len_bytes()));
        assert!(
            !all.iter().any(|(_, k)| *k == Highlight::Comment),
            "the comment is gone"
        );
    }

    #[test]
    fn ranges_limit_the_work_and_the_result() {
        let text = "fn a() {}\n".repeat(1000);
        let rope = Rope::from_str(&text);
        let syntax = Syntax::for_file(Path::new("a.rs"), &rope).unwrap();
        let hl = syntax.highlights(&rope, 500..520);
        assert!(!hl.is_empty());
        assert!(hl.iter().all(|(r, _)| r.start >= 500 && r.end <= 520));
    }

    #[test]
    fn outline_lists_definitions_with_kind_line_and_depth() {
        let text = "fn top() {\n    let x = 1;\n}\n\nstruct S {\n    field: u8,\n}\n\nimpl S {\n    fn method(&self) {}\n}\n";
        let rope = Rope::from_str(text);
        let syntax = Syntax::for_file(Path::new("a.rs"), &rope).unwrap();
        let out = syntax.outline(&rope);
        let listed: Vec<(&str, &str, usize, usize)> = out
            .iter()
            .map(|s| (s.name.as_str(), s.kind, s.line, s.depth))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("top", "function", 0, 0),
                ("S", "struct", 4, 0),
                ("impl S", "impl", 8, 0),
                ("method", "function", 9, 1),
            ]
        );
    }

    #[test]
    fn folds_come_from_the_tree_and_from_indentation() {
        let text = "fn a() {\n    one();\n    two();\n}\nstruct S {\n    x: u8,\n}\n";
        let rope = Rope::from_str(text);
        let syntax = Syntax::for_file(Path::new("a.rs"), &rope).unwrap();
        let folds = syntax.folds(&rope);
        assert!(folds.contains(&(0, 3)), "{folds:?}");
        assert!(folds.contains(&(4, 6)), "{folds:?}");

        // OCaml has no tree-sitter grammar here: indentation decides.
        let text = "let f x =\n  match x with\n  | 0 -> 1\n  | _ -> 2\n";
        let rope = Rope::from_str(text);
        let syntax = Syntax::for_file(Path::new("a.ml"), &rope).unwrap();
        assert!(!syntax.is_tree_sitter());
        assert_eq!(syntax.folds(&rope), vec![(0, 3)]);
    }
}
