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

use alloc::string::String;
use core::mem;
use core::option::Option;

use rustc_lexer::tokenize;

/// Outcome of a declaration scan.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Declaration {
    /// A top-level call to `renzora::script!(...)` was found outside
    /// any comment, string, byte string, raw string, char literal, or
    /// identifier continuation. The script is eligible to be loaded.
    Recognised,
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
    /// `renzora::script!(...)` somewhere the lexer treats as code (not a
    /// comment, string, byte string, raw string, char literal, or
    /// identifier suffix).
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
        // For the macro call `renzora::script!(` the buffer at the
        // OpenParen iteration holds:
        //   past[0] = Ident("renzora")
        //   past[1] = Colon
        //   past[2] = Colon
        //   past[3] = Ident("script")
        //   past[4] = Not ("!")
        // — five preceding tokens. The current token is OpenParen.
        // Comparing identifier text against the literal strings
        // "renzora" and "script" rules out same-length false positives.
        let mut past: [Option<TokenSlot>; 5] = Default::default();

        for token in tokenize(source) {
            // The current cursor is the byte offset for the new token:
            // past[4]'s end, or 0 if past[4] is empty.
            let cursor = past[4].as_ref().map(|t| t.end).unwrap_or(0);
            // Use checked_add so a malformed `len` cannot overflow.
            let Some(byte_end) = checked_add(cursor, token.len) else {
                return Declaration::NotRecognised;
            };
            let kind = TokenKind::from(&token.kind);
            let text = if matches!(kind, TokenKind::Ident) {
                source
                    .get(cursor..cursor.checked_add(token.len).unwrap_or(cursor))
                    .map(String::from)
            } else {
                None
            };
            // Check BEFORE shifting in. matches_five reads past as if
            // it contained `renzora :: script ! <current>` and rejects
            // anything where the four preceding tokens are not exactly
            // those, OR the current token is not OpenParen.
            if matches!(kind, TokenKind::OpenParen) && matches_five(&past, source) {
                return Declaration::Recognised;
            }

            // Now shift the new token in. Drop the oldest entry.
            let old1 = past[1].take();
            let old2 = past[2].take();
            let old3 = past[3].take();
            let old4 = past[4].take();
            let _ = past[0].take();
            past[0] = old1;
            past[1] = old2;
            past[2] = old3;
            past[3] = old4;
            past[4] = Some(TokenSlot {
                kind,
                offset: cursor,
                len: token.len,
                end: byte_end,
                text,
            });
        }
        Declaration::NotRecognised
    }
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
    /// Set when the lexer reports an `Ident` and the byte range is a
    /// valid UTF-8 substring. Other token kinds leave this `None`.
    text: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Ident,
    Colon,
    Not,
    OpenParen,
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
            _ => TokenKind::Other,
        }
    }
}

/// True when `past` matches `renzora` `::` `script` `!` followed by
/// `(`. The four preceding tokens are at `past[0..4]`; the current
/// `(`, already in `past[4]`, is checked at the call site.
fn matches_five(past: &[Option<TokenSlot>; 5], source: &str) -> bool {
    // past[0] = renzora, past[1] = colon, past[2] = colon,
    // past[3] = script, past[4] = open-paren.
    let p0 = match past[0].as_ref() { Some(t) => t, None => return false };
    let p1 = match past[1].as_ref() { Some(t) => t, None => return false };
    let p2 = match past[2].as_ref() { Some(t) => t, None => return false };
    let p3 = match past[3].as_ref() { Some(t) => t, None => return false };

    if p0.kind != TokenKind::Ident
        || p1.kind != TokenKind::Colon
        || p2.kind != TokenKind::Colon
        || p3.kind != TokenKind::Ident
    {
        return false;
    }
    // Verify the byte ranges are adjacent. Each token's byte offset
    // equals the previous token's end offset. UTF-8 boundaries are
    // guaranteed because the lexer emits valid UTF-8 byte sequences
    // and `&source[a..b]` panics only on non-char-boundary boundaries
    // (which the lexer never produces).
    if p0.offset.checked_add(p0.len) != Some(p1.offset) {
        return false;
    }
    if p1.offset.checked_add(p1.len) != Some(p2.offset) {
        return false;
    }
    if p2.offset.checked_add(p2.len) != Some(p3.offset) {
        return false;
    }
    // Identifier text comparison — not length-only. The lexer has
    // already validated the source as UTF-8 (Rust source files are
    // UTF-8 by convention), so slicing on byte offsets is safe.
    let renzora_text = match source.get(p0.offset..p0.end) {
        Some(s) => s,
        None => return false,
    };
    let script_text = match source.get(p3.offset..p3.end) {
        Some(s) => s,
        None => return false,
    };
    renzora_text == "renzora" && script_text == "script"
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
            Declaration::Recognised
        );
    }

    #[test]
    fn declaration_before_function_recognised() {
        assert_eq!(
            scan("renzora::script!(update);\nfn update() {}\n"),
            Declaration::Recognised
        );
    }

    #[test]
    fn leading_line_comments_recognised() {
        let src = "// hello\n// world\nfn update() {}\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::Recognised);
    }

    #[test]
    fn leading_block_comments_recognised() {
        let src = "/* hello\nworld */\nfn update() {}\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::Recognised);
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
        assert_eq!(scan(src), Declaration::Recognised);
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
        assert_eq!(scan(src), Declaration::Recognised);
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
        assert_eq!(scan(src), Declaration::Recognised);
    }

    #[test]
    fn no_declaration_returns_not_recognised() {
        let src = "fn ordinary_function() { let x = 1; }\n";
        assert_eq!(scan(src), Declaration::NotRecognised);
    }

    #[test]
    fn unicode_identifier_before_declaration_recognised() {
        let src = "fn 你好() {}\nrenzora::script!(你好);\n";
        assert_eq!(scan(src), Declaration::Recognised);
    }

    #[test]
    fn unicode_comment_before_declaration_recognised() {
        let src = "// コメント\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::Recognised);
    }

    #[test]
    fn identifier_continuation_in_middle_does_not_match() {
        let src = "fn update() {}\nlet _ = my_script_x;\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::Recognised);
    }

    /// State across two `scan` calls must not leak. Calling `scan`
    /// twice on the same recogniser must produce the same result as
    /// calling it twice on a fresh recogniser.
    #[test]
    fn state_does_not_leak_across_calls() {
        let rec = Recogniser::new();
        let _ = rec.scan("abcdefg::foobar!(x);\n");
        let res = rec.scan("renzora::script!(x);\n");
        assert_eq!(res, Declaration::Recognised);
    }
}
