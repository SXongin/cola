//! The fenced-code language token every card surface puts on a file's code
//! block: the File Viewer's preview and a Tool Panel's `read`/diff body.
//!
//! One table, because the two surfaces used to disagree: the same `.cs` file
//! highlighted in the viewer and rendered plain in its `read` panel.
//!
//! **The table is measured, not documentary.** Feishu's documented token list
//! is not the client's grammar registry. On a live card (2026-10-18) the
//! documented `c_sharp`, `protobuf`, `docker_file`, `cmake`, `makefile`,
//! `diff` and the short `ts` rendered with NO syntax colour, while the
//! undocumented `cs` and the short aliases `rs`, `md`, `js`, `py`, `sh`, `yml`
//! highlighted — and so did `typescript`. So a token is emitted here only
//! because it was measured to highlight (or because it is a documented
//! spelling we have no reason to doubt); the measured-broken ones are mapped
//! to [`PLAIN_TEXT`] instead.
//!
//! An unknown token is not an error — Feishu degrades it to plain text — so a
//! wrong entry costs the highlight, never the card.

/// What the platform renders when a code block carries no language it knows:
/// Feishu's own Plain Text token. Emitted for an extension with no honest
/// token, so the degradation is a named decision rather than an unlabelled
/// fence.
pub(crate) const PLAIN_TEXT: &str = "plain_text";

/// Tokens measured to render WITHOUT syntax colour on a live card, several of
/// them documented by Feishu as supported. The extension table must never
/// emit one: a working alias beats a documented spelling.
#[cfg(test)]
const BROKEN_TOKENS: &[&str] = &[
    "c_sharp",
    "protobuf",
    "docker_file",
    "cmake",
    "makefile",
    "diff",
    "ts",
];

/// The extension → language-token table (lowercased extension, no dot).
///
/// Deliberately absent — they render as [`PLAIN_TEXT`] — are the extensions
/// whose token is measured broken (`.cs`→`cs` instead of `c_sharp`,
/// `.proto`/`.dockerfile`/`.cmake`/`.mk`/`.diff` have no working token) and
/// the ones with no language at all (`.txt`, `.log`, `.ini`, `.cfg`, `.conf`,
/// `.env`).
const EXT_LANGUAGE: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("py", "python"),
    ("pyi", "python"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascript"),
    // `ts` is measured broken, `typescript` measured working — the long
    // spelling is the only one that highlights a TypeScript file.
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("go", "go"),
    ("java", "java"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("swift", "swift"),
    ("c", "c"),
    ("h", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "cpp"),
    ("hh", "cpp"),
    // `c_sharp` is measured broken; `cs` is the token the client highlights.
    ("cs", "cs"),
    ("rb", "ruby"),
    ("php", "php"),
    ("pl", "perl"),
    ("pm", "perl"),
    ("lua", "lua"),
    ("r", "r"),
    ("sql", "sql"),
    ("sh", "bash"),
    ("bash", "bash"),
    ("zsh", "shell"),
    ("ps1", "powershell"),
    ("psm1", "powershell"),
    ("json", "json"),
    ("toml", "toml"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("xml", "xml"),
    ("xsd", "xml"),
    ("svg", "xml"),
    ("html", "html"),
    ("htm", "html"),
    ("css", "css"),
    ("scss", "scss"),
    ("less", "css"),
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("diff", PLAIN_TEXT),
    ("patch", PLAIN_TEXT),
    ("proto", PLAIN_TEXT),
    ("dockerfile", PLAIN_TEXT),
    ("cmake", PLAIN_TEXT),
    ("mk", PLAIN_TEXT),
    ("ex", PLAIN_TEXT),
    ("txt", PLAIN_TEXT),
    ("log", PLAIN_TEXT),
    ("ini", PLAIN_TEXT),
    ("cfg", PLAIN_TEXT),
    ("conf", PLAIN_TEXT),
    ("env", PLAIN_TEXT),
    ("tex", "latex"),
    ("hs", "haskell"),
    ("erl", "erlang"),
    ("scala", "scala"),
    ("dart", "dart"),
    ("groovy", "groovy"),
    ("gradle", "groovy"),
    ("graphql", "graphql"),
    ("gql", "graphql"),
];

/// The language token for a file's extension, or [`PLAIN_TEXT`] when the
/// platform has no token for it.
pub(crate) fn language_token(ext: Option<&str>) -> &'static str {
    let Some(ext) = ext.map(str::to_ascii_lowercase) else {
        return PLAIN_TEXT;
    };
    EXT_LANGUAGE
        .iter()
        .find(|(candidate, _)| *candidate == ext)
        .map(|(_, token)| *token)
        .unwrap_or(PLAIN_TEXT)
}

/// The language token for a path's extension, or [`PLAIN_TEXT`] when the path
/// is extensionless (or has no honest token).
pub(crate) fn language_for_path(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str);
    language_token(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table entry answers for its own extension, case-insensitively,
    /// and no extension is listed twice.
    #[test]
    fn the_map_is_consistent() {
        for (ext, token) in EXT_LANGUAGE {
            assert_eq!(
                language_token(Some(ext)),
                *token,
                "`{ext}` must map to its own table entry"
            );
            let upper = ext.to_uppercase();
            assert_eq!(
                language_token(Some(&upper)),
                *token,
                "the match is case-insensitive (`{upper}`)"
            );
        }
        let mut exts: Vec<&str> = EXT_LANGUAGE.iter().map(|(e, _)| *e).collect();
        exts.sort();
        let before = exts.len();
        exts.dedup();
        assert_eq!(exts.len(), before, "no duplicate extension entries");
    }

    /// A token measured to render without colour must never be emitted, even
    /// where the platform documents it.
    #[test]
    fn the_map_never_emits_a_measured_broken_token() {
        for (ext, token) in EXT_LANGUAGE {
            assert!(
                !BROKEN_TOKENS.contains(token),
                "`{ext}` maps to `{token}`, which renders plain on a live card"
            );
        }
    }

    /// The measured aliases and the measured-broken spellings, pinned so a
    /// later "fix" toward the documented name cannot silently lose colour.
    #[test]
    fn measured_tokens_hold() {
        assert_eq!(language_token(Some("cs")), "cs", "`cs` highlights");
        assert_eq!(language_token(Some("ts")), "typescript", "`ts` does not");
        assert_eq!(language_token(Some("tsx")), "typescript");
        assert_eq!(language_token(Some("jsx")), "javascript");
        for ext in ["proto", "dockerfile", "cmake", "mk", "diff", "patch"] {
            assert_eq!(
                language_token(Some(ext)),
                PLAIN_TEXT,
                "`.{ext}` has no working token"
            );
        }
    }

    /// A path is read by its extension; extensionless and unknown paths stay
    /// plain instead of a near-miss highlight.
    #[test]
    fn a_path_reads_its_extension() {
        assert_eq!(language_for_path("src/main.rs"), "rust");
        assert_eq!(language_for_path("/x/y/README.MD"), "markdown");
        assert_eq!(language_for_path("Makefile"), PLAIN_TEXT);
        assert_eq!(language_for_path("noext"), PLAIN_TEXT);
        assert_eq!(language_for_path("archive.tar.gz"), PLAIN_TEXT);
        assert_eq!(language_token(None), PLAIN_TEXT);
    }
}
