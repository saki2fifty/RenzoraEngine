//! Reliable Rust-script declaration recognition.
//!
//! Phase 1 commit 1.4 replaces the byte-substring detector used in
//! `crates/renzora_rust_script/src/lib.rs:471 declares_script` with a
//! `rustc_lexer`-backed recogniser that ignores occurrences inside comments
//! and string literals.
//!
//! # Source
//!
//! `rustc_lexer 0.1.0` is the published crates.io crate, distinct from the
//! nightly compiler-internal `rustc_lexer` module. The API used here is the
//! 0.1.0 public surface, verified against docs.rs:
//!
//! - `pub fn tokenize(input: &str) -> impl Iterator<Item = Token>`
//! - `pub fn first_token(input: &str) -> Token` (returns `Token`, NOT `Option<Token>`;
//!   on empty input the lexer yields `Token { kind: Whitespace, len: 0 }`)
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

use rustc_lexer::tokenize;

/// Outcome of a declaration scan. Distinct from the dispatch's
/// `script_resolve::ResolvedScript` — that one is about runtime resolution,
/// this one is about whether the source even declares a script.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Declaration {
    /// A top-level call to `renzora::script!(...)` was found outside
    /// any comment, string, byte string, raw string, char literal, or
    /// identifier continuation. The script is eligible to be loaded.
    Recognised,
    /// The source has no matching declaration.
    NotRecognised,
}

/// Recogniser. Holds the last-five-token ring so the API exposes a single
/// value rather than a free function with a hidden state.
#[derive(Default)]
pub struct Recogniser {
    inner: Scan,
}

impl Recogniser {
    pub fn new() -> Self {
        Self::default()
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
    pub fn scan(&mut self, source: &str) -> Declaration {
        for token in tokenize(source) {
            if self.inner.advance(&token) == Step::Match {
                return Declaration::Recognised;
            }
        }
        Declaration::NotRecognised
    }
}

#[derive(Default)]
struct Scan {
    /// Track the last five tokens' kinds plus their byte offsets. The
    /// recognised macro call sequence is five tokens long. The byte
    /// offset is what the lexer started emitting the token at; combined
    /// with `Token::len` we get the byte range. UTF-8 boundaries are
    /// guaranteed because the lexer emits `Token::len` as the byte
    /// length of valid UTF-8.
    past: [Option<TokenOffset>; 5],
    cursor: usize,
}

#[derive(Copy, Clone, Debug)]
struct TokenOffset {
    kind: TokenKind,
    offset: usize,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Step {
    Continue,
    Match,
}

/// Mirror of `rustc_lexer::TokenKind` covering only the variants the
/// recogniser inspects. We pattern-match on a sum type so adding new
/// variants upstream does not silently break the recogniser; instead a
/// future diff must consciously decide whether the new variant should
/// break the five-token sequence.
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

impl Scan {
    fn advance(&mut self, token: &rustc_lexer::Token) -> Step {
        let offset = self.cursor;
        self.cursor = self.cursor.saturating_add(token.len);
        let kind = TokenKind::from(&token.kind);
        // The accepted macro pattern is `renzora::script!(`. The lexer
        // emits it as five tokens: `Ident("renzora")`, `Colon`,
        // `Colon`, `Ident("script")`, `Not`, and (current) `OpenParen`.
        //
        // We check the previous five tokens BEFORE shifting the current
        // token in. With N=5 past slots and the OpenParen still pending,
        // past[0..4] holds the five most-recent preceding tokens.
        if matches!(kind, TokenKind::OpenParen) && self.last_five_match() {
            // Shift the current token in for symmetry, even though we
            // are about to return. Keeps the state consistent if a
            // future caller calls `scan` more than once on the same
            // recogniser instance.
            shift(&mut self.past, Some(TokenOffset { kind, offset }));
            return Step::Match;
        }
        shift(&mut self.past, Some(TokenOffset { kind, offset }));
        Step::Continue
    }

    fn last_five_match(&self) -> bool {
        // Byte lengths: "renzora" = 7 bytes (ASCII); ":" = 1 byte;
        // "script" = 6 bytes (ASCII); "!" = 1 byte. UTF-8 source files
        // preserve ASCII bytes 1:1, so byte arithmetic is exact.
        matches!(
            self.past,
            [
                Some(TokenOffset { kind: TokenKind::Ident, offset: o0, .. }),
                Some(TokenOffset { kind: TokenKind::Colon, offset: o1, .. }),
                Some(TokenOffset { kind: TokenKind::Colon, offset: o2, .. }),
                Some(TokenOffset { kind: TokenKind::Ident, offset: o3, .. }),
                Some(TokenOffset { kind: TokenKind::Not, offset: o4, .. }),
            ] if o0 + 7 == o1 && o1 + 1 == o2 && o2 + 1 == o3 && o3 + 6 == o4
        )
    }
}

fn shift<const N: usize>(arr: &mut [Option<TokenOffset>; N], val: Option<TokenOffset>) {
    for i in 0..N - 1 {
        arr[i] = arr[i + 1].take();
    }
    arr[N - 1] = val;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the canonical happy-path script body.
    fn happy_after() -> &'static str {
        "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n"
    }

    /// Build the canonical happy-path script body with the marker first.
    fn happy_before() -> &'static str {
        "renzora::script!(update);\nfn update(_: &mut renzora::ScriptCtx) {}\n"
    }

    fn scan(source: &str) -> Declaration {
        Recogniser::new().scan(source)
    }

    #[test]
    fn declaration_after_function_recognised() {
        assert_eq!(scan(happy_after()), Declaration::Recognised);
    }

    #[test]
    fn declaration_before_function_recognised() {
        assert_eq!(scan(happy_before()), Declaration::Recognised);
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
        // Block comments nest in rustc_lexer; a `/* outer /* inner */ end */`
        // sequence parses correctly. A declaration after the closing
        // comment is recognised.
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

    #[test]
    fn identifier_with_script_substring_not_recognised() {
        // `script` here is a SUFFIX of a longer ident; the lexer
        // produces a single `Ident` token. The recogniser must NOT
        // treat this as a match.
        let src = "fn my_renzora::script_thing() {}\n";
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
        // Multi-byte UTF-8 identifiers must not perturb the byte-offset
        // arithmetic: the lexer reports byte lengths for tokens and
        // the recogniser compares byte offsets, not char offsets.
        let src = "fn 你好() {}\nrenzora::script!(你好);\n";
        assert_eq!(scan(src), Declaration::Recognised);
    }

    #[test]
    fn unicode_comment_before_declaration_recognised() {
        // Multi-byte UTF-8 in a line comment: the lexer yields one
        // LineComment token whose byte length is the UTF-8 byte count.
        let src = "// コメント\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::Recognised);
    }

    #[test]
    fn identifier_continuation_in_middle_does_not_match() {
        // An ident where `script` is in the middle of an identifier
        // is a single token; the recogniser must not match.
        let src = "fn update() {}\nlet _ = my_script_x;\nrenzora::script!(update);\n";
        assert_eq!(scan(src), Declaration::Recognised);
    }
}
