//! Loose-file authoring contract parser.
//!
//! A loose plugin file contains one or more `renzora_plugin::add!(...)`
//! declarations. This module extracts the plugin type and the optional scope
//! (`Runtime` or `Editor`) from those declarations and refuses anything else.
//!
//! Rejections are explicit and surface in the inventory as a non-recoverable
//! state — see [`LoosePluginStatusKind::WrongScope`] and
//! [`LoosePluginParseError`]. The host never silently classifies a malformed
//! file as Runtime; the parser either returns a clean contract or the file
//! is excluded from compilation with a recorded diagnostic.

use std::fmt;

/// What a loose file is allowed to declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoosePluginScope {
    Runtime,
    Editor,
}

impl LoosePluginScope {
    pub fn as_label(self) -> &'static str {
        match self {
            LoosePluginScope::Runtime => "Runtime",
            LoosePluginScope::Editor => "Editor",
        }
    }
}

impl fmt::Display for LoosePluginScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_label())
    }
}

/// One parsed declaration of a loose plugin.
///
/// More than one `add!(…)` in a file is a contract violation: two plugins
/// sharing one source file have nowhere to register independently, and a
/// single file with two scopes is what Phase 3 explicitly forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoosePluginContract {
    /// The plugin type identifier as it appeared in the `add!` macro call.
    /// Recorded verbatim; the host does not require a specific naming
    /// convention.
    pub plugin_type: String,
    pub scope: LoosePluginScope,
}

/// Reasons a file cannot be turned into a [`LoosePluginContract`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoosePluginParseError {
    /// No `renzora_plugin::add!(...)` declaration was found.
    NoAddMacro,
    /// `add!(...)` was present with no scope argument.
    Unscoped,
    /// The scope argument was something other than `Runtime` or `Editor`.
    /// `Both` is rejected with a separate variant because it is the one a
    /// Phase 3 author is most likely to mis-type.
    UnknownScope(String),
    /// Two or more `add!(...)` declarations in one file. Each file must
    /// declare exactly one plugin.
    ConflictingDeclarations,
    /// Two or more `add!(...)` declarations had different scopes. Phase 3
    /// rejects "Both" by rejecting any multi-add file with a scope conflict.
    ConflictingScopes,
}

impl fmt::Display for LoosePluginParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoosePluginParseError::NoAddMacro => {
                f.write_str("no `renzora_plugin::add!(...)` declaration found")
            }
            LoosePluginParseError::Unscoped => {
                f.write_str("`renzora_plugin::add!(...)` declared no scope — explicit `Runtime` or `Editor` is required")
            }
            LoosePluginParseError::UnknownScope(s) => {
                write!(f, "scope `{s}` is not one of `Runtime` or `Editor`")
            }
            LoosePluginParseError::ConflictingDeclarations => {
                f.write_str("file contains more than one `renzora_plugin::add!(...)` declaration")
            }
            LoosePluginParseError::ConflictingScopes => {
                f.write_str("`renzora_plugin::add!(...)` declarations have conflicting scopes")
            }
        }
    }
}

impl std::error::Error for LoosePluginParseError {}

/// Find every `renzora_plugin::add!(...)` call site in `source` and parse
/// it into a contract.
///
/// Returns `Err` for any of the [`LoosePluginParseError`] cases. Multiple
/// `add!` declarations are always rejected — one file, one plugin — which is
/// what the design says authors who want both scopes should split into two
/// files.
pub fn parse_loose_plugin_source(
    source: &[u8],
) -> Result<LoosePluginContract, LoosePluginParseError> {
    let text = match std::str::from_utf8(source) {
        Ok(s) => s,
        Err(_) => {
            // Non-UTF-8 source: refuse without claiming the macro is missing;
            // the diagnostic surfaces elsewhere.
            return Err(LoosePluginParseError::NoAddMacro);
        }
    };

    let mut declarations: Vec<LoosePluginContract> = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = find_add_macro(&text[search_from..]) {
        let abs = search_from + rel.call_open;
        let call_end = search_from + rel.call_close;
        let args_text = &text[abs + 1..call_end];
        let parsed = parse_add_arguments(args_text)?;
        declarations.push(parsed);
        search_from = call_end + 1;
    }

    match declarations.len() {
        0 => Err(LoosePluginParseError::NoAddMacro),
        1 => Ok(declarations.into_iter().next().expect("len == 1")),
        n if n > 1 => {
            // Two or more. If they all agree on scope and plugin type, the
            // author probably meant the same plugin twice — still wrong, and
            // we refuse for the same reason. Distinct scopes are reported
            // with the more specific variant.
            let first = &declarations[0];
            if declarations.iter().skip(1).any(|d| d.scope != first.scope) {
                Err(LoosePluginParseError::ConflictingScopes)
            } else {
                Err(LoosePluginParseError::ConflictingDeclarations)
            }
        }
        _ => unreachable!(),
    }
}

/// One `add!(...)` invocation, narrowed down to the byte offsets of the
/// argument list.
struct AddMacroSite {
    call_open: usize,
    call_close: usize,
}

/// Find the next `renzora_plugin::add!(` site in `text`, scanning forward
/// from byte 0. Returns the byte offsets of the opening paren of the macro
/// call and the matching closing paren.
///
/// Skips line and block comments and string/char literals so a `add!`
/// reference inside a doc comment does not get parsed.
fn find_add_macro(text: &str) -> Option<AddMacroSite> {
    let needle = "renzora_plugin::add!(";
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle.as_bytes() {
            let call_open = i + needle.len() - 1; // position of '('
                                                  // Find the matching ')' by balanced scan that respects nested
                                                  // parens, comments, and string literals.
            let call_close = find_matching_close(text, call_open)?;
            return Some(AddMacroSite {
                call_open,
                call_close,
            });
        }
        // Skip one character, respecting comments and literals.
        i += skip_one_token(bytes, i);
    }
    None
}

/// How many bytes to advance past the character at `i`, treating line/block
/// comments and char/string literals as their own units.
fn skip_one_token(bytes: &[u8], i: usize) -> usize {
    let b = bytes[i];
    match b {
        b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
            // line comment
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            j - i
        }
        b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
            // block comment
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            (j + 2).min(bytes.len()) - i
        }
        b'"' => skip_string(bytes, i),
        b'\'' => skip_char(bytes, i),
        _ => 1,
    }
}

fn skip_string(bytes: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => {
                j = (j + 2).min(bytes.len());
            }
            b'"' => {
                j += 1;
                break;
            }
            _ => j += 1,
        }
    }
    j - i
}

fn skip_char(bytes: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => {
                j = (j + 2).min(bytes.len());
            }
            b'\'' => {
                j += 1;
                break;
            }
            _ => j += 1,
        }
    }
    j - i
}

/// Find the byte offset of the `)` that matches the `(` at `open`, scanning
/// forward and counting nested parens while respecting comments/literals.
fn find_matching_close(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    debug_assert_eq!(bytes[open], b'(');
    let mut depth: usize = 1;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += skip_one_token(bytes, i),
        }
    }
    None
}

/// Split `args_text` (the inside of an `add!(...)` call) into the plugin
/// type token and the optional scope token.
fn parse_add_arguments(args_text: &str) -> Result<LoosePluginContract, LoosePluginParseError> {
    // Split top-level commas only — nested parens/braces are allowed in the
    // plugin-type token (e.g. `MyPlugin<X, Y>`), but Phase 3 authors are not
    // expected to write that, and a more permissive split is harder to read.
    let parts: Vec<&str> = split_top_level_commas(args_text)
        .into_iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    match parts.len() {
        1 => {
            // Single argument = the plugin type; scope is missing.
            let plugin_type = strip_trailing_punct(parts[0]).to_string();
            let _ = plugin_type; // keep the field for any future "unscoped default" diagnostic
            Err(LoosePluginParseError::Unscoped)
        }
        2 => {
            let plugin_type = strip_trailing_punct(parts[0]).to_string();
            let scope = match parts[1] {
                "Runtime" => LoosePluginScope::Runtime,
                "Editor" => LoosePluginScope::Editor,
                other => return Err(LoosePluginParseError::UnknownScope(other.to_string())),
            };
            Ok(LoosePluginContract { plugin_type, scope })
        }
        n if n > 2 => Err(LoosePluginParseError::UnknownScope(format!(
            "{} extra arguments after the plugin type",
            n - 1
        ))),
        0 => Err(LoosePluginParseError::Unscoped),
        _ => unreachable!(),
    }
}

fn split_top_level_commas(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut depth_paren = 0usize;
    let mut depth_angle = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth_paren += 1,
            b')' => depth_paren = depth_paren.saturating_sub(1),
            b'<' => depth_angle += 1,
            b'>' => depth_angle = depth_angle.saturating_sub(1),
            b',' if depth_paren == 0 && depth_angle == 0 => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += skip_one_token(bytes, i);
    }
    out.push(&text[start..]);
    out
}

fn strip_trailing_punct(s: &str) -> &str {
    s.trim_end_matches(|c: char| c == ';' || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runtime_scope() {
        let src = b"renzora_plugin::add!(SpinPlugin, Runtime);";
        let c = parse_loose_plugin_source(src).expect("parses");
        assert_eq!(c.plugin_type, "SpinPlugin");
        assert_eq!(c.scope, LoosePluginScope::Runtime);
    }

    #[test]
    fn parses_editor_scope() {
        let src = b"renzora_plugin::add!(InspectorPanel, Editor);";
        let c = parse_loose_plugin_source(src).expect("parses");
        assert_eq!(c.plugin_type, "InspectorPanel");
        assert_eq!(c.scope, LoosePluginScope::Editor);
    }

    #[test]
    fn rejects_unscoped() {
        let src = b"renzora_plugin::add!(SpinPlugin);";
        let err = parse_loose_plugin_source(src).expect_err("unscoped");
        assert_eq!(err, LoosePluginParseError::Unscoped);
    }

    #[test]
    fn rejects_unknown_scope() {
        let src = b"renzora_plugin::add!(SpinPlugin, Both);";
        let err = parse_loose_plugin_source(src).expect_err("both rejected");
        assert!(matches!(err, LoosePluginParseError::UnknownScope(_)));
    }

    #[test]
    fn rejects_no_add_macro() {
        let src = b"fn not_a_plugin() {}";
        let err = parse_loose_plugin_source(src).expect_err("missing");
        assert_eq!(err, LoosePluginParseError::NoAddMacro);
    }

    #[test]
    fn rejects_conflicting_declarations() {
        let src = b"renzora_plugin::add!(A, Runtime); renzora_plugin::add!(B, Runtime);";
        let err = parse_loose_plugin_source(src).expect_err("two");
        assert_eq!(err, LoosePluginParseError::ConflictingDeclarations);
    }

    #[test]
    fn rejects_conflicting_scopes() {
        let src = b"renzora_plugin::add!(A, Runtime); renzora_plugin::add!(B, Editor);";
        let err = parse_loose_plugin_source(src).expect_err("scopes");
        assert_eq!(err, LoosePluginParseError::ConflictingScopes);
    }

    #[test]
    fn ignores_add_macro_inside_string_literal() {
        let src = br#"
            fn example() {
                let _ = "renzora_plugin::add!(NotAPlugin, Runtime)";
            }
        "#;
        let err = parse_loose_plugin_source(src).expect_err("string match is not real");
        assert_eq!(err, LoosePluginParseError::NoAddMacro);
    }

    #[test]
    fn ignores_add_macro_inside_line_comment() {
        let src = b"// renzora_plugin::add!(Note, Runtime)\nfn x() {}";
        let err = parse_loose_plugin_source(src).expect_err("comment match is not real");
        assert_eq!(err, LoosePluginParseError::NoAddMacro);
    }
}
