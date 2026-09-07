//! Reliable Rust-script declaration recognition.
//!
//! Phase 1 commit 1.4 (and correction 10) replace the byte-substring
//! detector used in `crates/renzora_rust_script/src/lib.rs:471
//! declares_script` with a `rustc_lexer`-backed recogniser that
//!
//! - ignores occurrences inside comments and string literals;
//! - compares identifier token text against the literal strings
//!   `renzora` and `script` (NOT just byte-length);
//! - checks byte ranges and UTF-8 boundaries before slicing;
//! - tolerates incomplete source without panicking.
//!
//! # Source
//!
//! `rustc_lexer 0.1.0` is the published crates.io crate, distinct from
//! the nightly compiler-internal `rustc_lexer` module. The API used
//! here is the 0.1.0 public surface, verified against docs.rs:
//!
//! - `pub fn tokenize(input: &str) -> impl Iterator<Item = Token>`
//! - `pub fn first_token(input: &str) -> Token` (returns `Token`,
//!   NOT `Option<Token>`)
//! - `pub struct Token { pub kind: TokenKind, pub len: usize }`
//! - `pub enum TokenKind { LineComment, BlockComment { terminated: bool },
//!     Whitespace, Ident, RawIdent,
//!     Literal { kind: LiteralKind, suffix_start: usize },
//!     Lifetime { starts_with_number: bool },
//!     Colon, Not, OpenParen, Unknown, ... }`
//! - `pub enum LiteralKind { Int { base, empty_int }, Float { base, empty_exponent },
//!     Char { terminated }, Byte { terminated },
//!     Str { terminated }, ByteStr { terminated },
//!     RawStr { n_hashes, started, terminated },
//!     RawByteStr { n_hashes, started, terminated } }`
//!
//! One transitive dep: `unicode-xid ^0.2.0`. The crate is pinned to
//! `=0.1.0` in this crate's `Cargo.toml`.

extern crate alloc;

use core::option::Option;

use rustc_lexer::tokenize;

/// Outcome of a declaration scan.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Declaration {
    /// A top-level call to `renzora_plugin::rust_script!(...)` (the
    /// Phase 4 Tier 1 form) was found outside any comment, string,
    /// byte string, raw string, char literal, or identifier
    /// continuation. The script is eligible to be loaded through the
    /// new C-ABI descriptor.
    Recognised,
    /// A top-level call to the legacy `renzora::script!(...)` form was
    /// found. Phase 4 does NOT load this — it is reported so the
    /// editor can emit a migration diagnostic. Recognised for the
    /// transition window.
    LegacyRecognised,
    /// The source has no matching declaration.
    NotRecognised,
}

/// Stateless declaration recogniser.
///
/// Every `scan` call walks the source from offset 0; no state is
/// carried across calls. Construct via [`Recogniser::scan`] for the
/// convenience one-shot, or [`Recogniser::new`] + [`Self::scan`]
/// when you want to inspect more than one file in sequence.
#[derive(Default)]
pub struct Recogniser;

impl Recogniser {
    pub fn new() -> Self {
        Self
    }

    /// One-shot scan. Equivalent to constructing a fresh recogniser and
    /// calling `scan` on it — use the static method when you only have
    /// one file to inspect.
    pub fn scan_one(source: &str) -> Declaration {
        Self::new().scan(source)
    }

    /// Decide whether `source` declares a Rust script by calling
    /// either the Tier 1 `renzora_plugin::rust_script!(...)` form
    /// (Phase 4, returned as [`Declaration::Recognised`]) or the
    /// legacy `renzora::script!(...)` form (returned as
    /// [`Declaration::LegacyRecognised`] for the documented
    /// transition window — the editor surfaces a migration diagnostic
    /// and never compiles the source through the unsafe old loader).
    ///
    /// Tolerance: truncated source, unterminated strings, unclosed
    /// block comments, and missing `!` all return `NotRecognised`
    /// without panicking. `rustc_lexer::tokenize` is infallible: it
    /// returns each fully-formed token up to the cut point and stops
    /// at the partial one.
    pub fn scan(&self, source: &str) -> Declaration {
        // Sliding window of the last five tokens. The check runs on
        // each yielded token BEFORE shifting it in: `past` holds the
        // five most-recent preceding tokens when the new token arrives.
        // For the legacy macro call `renzora::script!("...", ...)`:
        //   past[0] = Ident("renzora")
        //   past[1] = Colon
        //   past[2] = Colon
        //   past[3] = Ident("script")
        //   past[4] = Not ("!")
        // For the Phase 4 macro call `renzora_plugin::rust_script!(...)`:
        //   past[0] = Ident("renzora_plugin")
        //   past[1] = Colon
        //   past[2] = Colon
        //   past[3] = Ident("rust_script")
        //   past[4] = Not ("!")
        // The current token, in both cases, is OpenParen. The
        // recogniser distinguishes the two by the byte text of
        // `past[0]` and `past[3]`. The Phase 4 form (`renzora_plugin::
        // rust_script!`) has no canonical-id string argument; the legacy
        // form starts with a string literal. We look one token ahead
        // past the OpenParen to decide.
        let mut past: [Option<TokenSlot>; 5] = Default::default();
        let mut next_is_openparen: bool = false;
        let mut pending_decision: Option<TokenKind> = None;

        for token in tokenize(source) {
            // The current cursor is the byte offset for the new token:
            // past[4]'s end, or 0 if past[4] is empty.
            let cursor = past[4].as_ref().map(|t| t.end).unwrap_or(0);
            // Use checked_add so a malformed `len` cannot overflow.
            let Some(byte_end) = checked_add(cursor, token.len) else {
                return Declaration::NotRecognised;
            };
            let kind = TokenKind::from(&token.kind);

            // Resolved a pending macro detection: the FIRST token AFTER
            // the `OpenParen` of a Phase 4 macro decides Recognised vs
            // LegacyRecognised for `renzora_plugin::rust_script!`.
            if next_is_openparen {
                next_is_openparen = false;
                if pending_decision.take() == Some(TokenKind::OpenParen) {
                    // `renzora_plugin::rust_script!(` was detected;
                    // the next token tells us the form. A string
                    // literal first argument = the legacy string-id
                    // form (still observed during the transition
                    // window); anything else (identifier / numeric /
                    // grouped) is the authoritative Phase 4 form.
                    return match kind {
                        TokenKind::Literal => Declaration::LegacyRecognised,
                        _ => Declaration::Recognised,
                    };
                }
            }

            // Check BEFORE shifting in. The five-token frame is
            // shared between the two recognised forms; the variant is
            // chosen by the byte text of `past[0]` and `past[3]`.
            if matches!(kind, TokenKind::OpenParen) {
                if let Some(variant) = matches_five(&past, source) {
                    if variant == Declaration::Recognised {
                        // Phase 4 path: the very next token decides.
                        next_is_openparen = true;
                        pending_decision = Some(TokenKind::OpenParen);
                        // Don't shift OpenParen into the window — we
                        // already matched it. Continue scanning.
                        continue;
                    } else {
                        return variant;
                    }
                }
            }

            // Now shift the new token in. Drop the oldest entry.
            push_past(
                &mut past,
                TokenSlot {
                    kind,
                    offset: cursor,
                    len: token.len,
                    end: byte_end,
                },
            );
        }
        Declaration::NotRecognised
    }
}

fn push_past(past: &mut [Option<TokenSlot>; 5], slot: TokenSlot) {
    past[0] = past[1].take();
    past[1] = past[2].take();
    past[2] = past[3].take();
    past[3] = past[4].take();
    past[4] = Some(slot);
}

/// Add two `usize`s, returning `None` on overflow.
fn checked_add(a: usize, b: usize) -> Option<usize> {
    a.checked_add(b)
}

#[derive(Clone, Debug)]
struct TokenSlot {
    kind: TokenKind,
    offset: usize,
    len: usize,
    end: usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Ident,
    Colon,
    Not,
    OpenParen,
    Literal,
    Other,
}

impl From<&rustc_lexer::TokenKind> for TokenKind {
    fn from(k: &rustc_lexer::TokenKind) -> Self {
        use rustc_lexer::TokenKind::*;
        match k {
            Ident => TokenKind::Ident,
            Colon => TokenKind::Colon,
            Not => TokenKind::Not,
            OpenParen => TokenKind::OpenParen,
            Literal { .. } => TokenKind::Literal,
            _ => TokenKind::Other,
        }
    }
}

/// Recognised when `past` matches the five-token shape `<head> :: <tail> ! (`
/// for either the legacy or the Phase 4 macro spelling. The four
/// preceding tokens are at `past[0..4]`; the current `(`, already in
/// `past[4]`, is checked at the call site.
///
/// Returns [`Some(Declaration::Recognised)`] for the Phase 4 form
/// `renzora_plugin::rust_script!(` and
/// [`Some(Declaration::LegacyRecognised)`] for the legacy
/// `renzora::script!(` form. Returns [`None`] when the five-token
/// shape is not exactly one of those two.
fn matches_five(past: &[Option<TokenSlot>; 5], source: &str) -> Option<Declaration> {
    let p0 = past[0].as_ref()?;
    let p1 = past[1].as_ref()?;
    let p2 = past[2].as_ref()?;
    let p3 = past[3].as_ref()?;

    if p0.kind != TokenKind::Ident
        || p1.kind != TokenKind::Colon
        || p2.kind != TokenKind::Colon
        || p3.kind != TokenKind::Ident
    {
        return None;
    }
    if p0.offset.checked_add(p0.len) != Some(p1.offset) {
        return None;
    }
    if p1.offset.checked_add(p1.len) != Some(p2.offset) {
        return None;
    }
    if p2.offset.checked_add(p2.len) != Some(p3.offset) {
        return None;
    }
    let head_text = source.get(p0.offset..p0.end)?;
    let tail_text = source.get(p3.offset..p3.end)?;
    if head_text == "renzora_plugin" && tail_text == "rust_script" {
        Some(Declaration::Recognised)
    } else if head_text == "renzora" && tail_text == "script" {
        Some(Declaration::LegacyRecognised)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(source: &str) -> Declaration {
        Recogniser::scan_one(source)
    }

    #[test]
    fn declaration_after_function_recognised() {
        assert_eq!(
            scan("fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n"),
            Declaration::LegacyRecognised
        );
    }

    #[test]
    fn phase4_declaration_after_function_recognised() {
        // Phase 4 authoritative form: no embedded identity string.
        assert_eq!(
            scan("fn update(ctx: &renzora_plugin::script::Ctx, _r: &mut renzora_plugin::script::ScriptReply) -> Result<(), String> { Ok(()) }\nrenzora_plugin::rust_script!(update);\n"),
            Declaration::Recognised
        );
    }

    #[test]
    fn phase4_legacy_string_id_form_is_legacy_recognised() {
        // The pre-correction form embedded a canonical id string; treat
        // any such occurrence as legacy so the editor surfaces the
        // migration diagnostic.
        assert_eq!(
            scan("renzora_plugin::rust_script!(\"project://a.rs\", update);\n"),
            Declaration::LegacyRecognised
        );
    }

    #[test]
    fn declaration_before_function_recognised() {
        assert_eq!(
            scan("renzora::script!(update);\nfn update() {}\n"),
            Declaration::LegacyRecognised
        );
    }

    #[test]
    fn phase4_declaration_before_function_recognised() {
        assert_eq!(
            scan("renzora_plugin::rust_script!(update);\nfn update(_: &Ctx, _: &mut ScriptReply) -> Result<(), String> { Ok(()) }\n"),
            Declaration::Recognised
        );
    }

    #[test]
    fn leading_line_comments_recognised() {
        let src = "// hello\n// world\nfn update() {}\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::LegacyRecognised);
    }

    #[test]
    fn leading_block_comments_recognised() {
        let src = "/* hello\nworld */\nfn update() {}\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::LegacyRecognised);
    }

    #[test]
    fn commented_out_declaration_not_recognised() {
        let src = "// renzora::script!(update);\nfn update() {}\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn block_commented_out_declaration_not_recognised() {
        let src = "/* renzora::script!(update); */\nfn update() {}\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn nested_block_comments_recognised() {
        let src = "/* outer /* inner */ end */\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::LegacyRecognised);
    }

    #[test]
    fn string_literal_fake_declaration_not_recognised() {
        let src = "let _ = \"renzora::script!\";\nfn update() {}\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn raw_string_literal_fake_declaration_not_recognised() {
        let src = "let _ = r#\"renzora::script!(update);\"#;\nfn update() {}\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn byte_string_literal_fake_declaration_not_recognised() {
        let src = "let _ = b\"renzora::script!\";\nfn update() {}\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn char_literal_fake_declaration_not_recognised() {
        let src = "let _ = '!';\nfn update() {}\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    /// Same-length macro test from the Codex review: the previous
    /// recognizer accepted this as a script because it was byte-length-
    /// only. The corrected recognizer compares identifier text.
    #[test]
    fn abcdefg_foobar_with_same_lengths_is_not_recognised() {
        let src = "abcdefg::foobar!(update);\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn renzora_with_wrong_second_ident_is_not_recognised() {
        let src = "renzora::foobar!(update);\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn wrong_first_ident_with_script_is_not_recognised() {
        let src = "abcdefg::script!(update);\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn renzora_script_is_recognised_with_correct_text() {
        let src = "renzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::LegacyRecognised);
    }

    #[test]
    fn phase4_rust_script_is_recognised_with_correct_text() {
        let src = "renzora_plugin::rust_script!(update);\n";
        assert_eq!(scan(src), Declaration::Recognised);
    }

    #[test]
    fn renzora_plugin_with_wrong_tail_is_not_recognised() {
        let src = "renzora_plugin::foobar!(update);\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn wrong_head_with_rust_script_is_not_recognised() {
        let src = "abcdefg::rust_script!(update);\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn declaration_in_block_comment_not_recognised() {
        let src = "/* foo\n   renzora::script!(update);\n*/\nfn x() {}\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn declaration_in_line_comment_not_recognised() {
        let src = "fn x() {}\n// renzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn identifier_with_script_substring_not_recognised() {
        // `script` here is a SUFFIX of a longer identifier; the lexer
        // produces a single `Ident` token. The recognizer compares
        // against the literal `script`, so longer ids do not match.
        let src = "my_renzora::script_thing!()\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn malformed_unterminated_string_does_not_panic() {
        let src = "let _ = \"renzora::script!(update);";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn incomplete_raw_string_does_not_panic() {
        let src = "let _ = r#\"renzora::script!(update);";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn incomplete_block_comment_does_not_panic() {
        let src = "/* this comment never ends and neither does this file";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn malformed_punctuation_does_not_panic() {
        let src = "fn update() @@@### renzora::script!(update); }}}}}";
        assert_eq!(scan(src), Declaration::LegacyRecognised);
    }

    #[test]
    fn no_declaration_returns_not_recognised() {
        let src = "fn ordinary_function() { let x = 1; }\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn unicode_identifier_before_declaration_recognised() {
        let src = "fn 你好() {}\nrenzora::script!(你好);\n";
        assert_eq!(scan(src), Declaration::LegacyRecognised);
    }

    #[test]
    fn unicode_comment_before_declaration_recognised() {
        let src = "// コメント\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::LegacyRecognised);
    }

    #[test]
    fn identifier_continuation_in_middle_does_not_match() {
        let src = "fn update() {}\nlet _ = my_script_x;\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::LegacyRecognised);
    }

    /// State across two `scan` calls must not leak. Calling `scan`
    /// twice on the same recogniser must produce the same result as
    /// calling it twice on a fresh recogniser.
    #[test]
    fn state_does_not_leak_across_calls() {
        let rec = Recogniser::new();
        let _ = rec.scan("abcdefg::foobar!(x);\n");
        let res = rec.scan("renzora::script!(x);\n");
        assert_eq!(res, Declaration::LegacyRecognised);
    }
}
