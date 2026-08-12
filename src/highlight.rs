//! A small, dependency-free line tokenizer for the file viewer.
//!
//! Not a real syntax highlighter: no cross-line state (a triple-quoted Python
//! string or a `/* */` block comment spanning lines highlights per line, not
//! as a whole), no full grammars. `SPEC.md` keeps ChipTUI "small, reliable
//! and scriptable" and explicitly out of the source-editor business (§3), so
//! this covers keywords/strings/comments/numbers for the languages a
//! MicroPython or Zephyr project actually contains, rather than pulling in a
//! general-purpose engine.

/// Recognised from the file's name alone (extension), not content --- the
/// only signal available before a device file has even been fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    C,
    Rust,
    Json,
    Toml,
    Yaml,
    Shell,
    Markdown,
    PlainText,
}

impl Language {
    pub fn from_filename(name: &str) -> Self {
        match name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
        {
            Some(ext) => match ext.as_str() {
                "py" => Self::Python,
                "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "hxx" | "ino" => Self::C,
                "rs" => Self::Rust,
                "json" => Self::Json,
                "toml" => Self::Toml,
                "yaml" | "yml" => Self::Yaml,
                "sh" | "bash" => Self::Shell,
                "md" | "markdown" => Self::Markdown,
                _ => Self::PlainText,
            },
            None => Self::PlainText,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Plain,
    Keyword,
    String,
    Comment,
    Number,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub kind: TokenKind,
}

/// Tokenizes one line. Every language falls back to a single [`TokenKind::Plain`]
/// token ([`Language::PlainText`], and anything [`Language::from_filename`]
/// did not recognise) rather than a special "no highlighting" path, so
/// callers never need to branch on whether highlighting applies.
pub fn highlight_line(line: &str, language: Language) -> Vec<Token> {
    match language {
        Language::Markdown => highlight_markdown_line(line),
        Language::PlainText => vec![Token {
            text: line.to_string(),
            kind: TokenKind::Plain,
        }],
        _ => generic_scan(line, &config(language)),
    }
}

struct LangConfig {
    keywords: &'static [&'static str],
    line_comment: Option<&'static str>,
}

fn config(language: Language) -> LangConfig {
    match language {
        Language::Python => LangConfig {
            keywords: &[
                "def", "class", "import", "from", "as", "return", "if", "elif", "else", "for",
                "while", "in", "not", "and", "or", "is", "None", "True", "False", "try", "except",
                "finally", "with", "lambda", "pass", "break", "continue", "yield", "global",
                "nonlocal", "raise", "assert", "del", "async", "await",
            ],
            line_comment: Some("#"),
        },
        Language::C => LangConfig {
            keywords: &[
                "int", "char", "float", "double", "void", "short", "long", "unsigned", "signed",
                "struct", "union", "enum", "typedef", "static", "const", "extern", "volatile",
                "if", "else", "for", "while", "do", "switch", "case", "default", "break",
                "continue", "return", "sizeof", "goto", "include", "define", "ifdef", "ifndef",
                "endif", "pragma",
            ],
            line_comment: Some("//"),
        },
        Language::Rust => LangConfig {
            keywords: &[
                "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "use", "mod",
                "match", "if", "else", "for", "while", "loop", "return", "break", "continue",
                "self", "Self", "true", "false", "const", "static", "async", "await", "move",
                "ref", "where", "dyn", "unsafe", "as", "in", "crate", "super",
            ],
            line_comment: Some("//"),
        },
        Language::Json => LangConfig {
            keywords: &["true", "false", "null"],
            line_comment: None,
        },
        Language::Toml => LangConfig {
            keywords: &["true", "false"],
            line_comment: Some("#"),
        },
        Language::Yaml => LangConfig {
            keywords: &["true", "false", "null"],
            line_comment: Some("#"),
        },
        Language::Shell => LangConfig {
            keywords: &[
                "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac",
                "function", "return", "export", "local", "exit",
            ],
            line_comment: Some("#"),
        },
        Language::Markdown | Language::PlainText => LangConfig {
            keywords: &[],
            line_comment: None,
        },
    }
}

/// Shared by every language but Markdown/plain text: an identifier-or-keyword
/// scanner plus quoted strings, `//`/`#`-style line comments and numbers.
/// Works on `char`s rather than byte offsets, matching how `ui/files.rs`
/// already handles non-ASCII names --- string slicing on a byte index that
/// lands mid-codepoint would panic.
fn generic_scan(line: &str, cfg: &LangConfig) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if let Some(prefix) = cfg.line_comment
            && matches_at(&chars, i, prefix)
        {
            tokens.push(Token {
                text: chars[i..].iter().collect(),
                kind: TokenKind::Comment,
            });
            break;
        }

        let c = chars[i];

        if c == '"' || c == '\'' {
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                let closed = chars[i] == c;
                i += 1;
                if closed {
                    break;
                }
            }
            tokens.push(Token {
                text: chars[start..i].iter().collect(),
                kind: TokenKind::String,
            });
            continue;
        }

        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            tokens.push(Token {
                text: chars[start..i].iter().collect(),
                kind: TokenKind::Number,
            });
            continue;
        }

        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let kind = if cfg.keywords.contains(&word.as_str()) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            };
            tokens.push(Token { text: word, kind });
            continue;
        }

        // A run of whatever is left --- whitespace and punctuation --- glued
        // into one token instead of one per character.
        let start = i;
        while i < chars.len() {
            let c = chars[i];
            if c.is_alphanumeric() || c == '_' || c == '"' || c == '\'' {
                break;
            }
            if let Some(prefix) = cfg.line_comment
                && matches_at(&chars, i, prefix)
            {
                break;
            }
            i += 1;
        }
        tokens.push(Token {
            text: chars[start..i].iter().collect(),
            kind: TokenKind::Plain,
        });
    }

    tokens
}

fn matches_at(chars: &[char], i: usize, prefix: &str) -> bool {
    let prefix: Vec<char> = prefix.chars().collect();
    i + prefix.len() <= chars.len() && chars[i..i + prefix.len()] == prefix[..]
}

/// Markdown gets its own pass instead of [`generic_scan`]: its "keywords" are
/// structural (headings), not a fixed word list, and its only inline span
/// worth marking here is backtick code.
fn highlight_markdown_line(line: &str) -> Vec<Token> {
    if line.trim_start().starts_with('#') {
        return vec![Token {
            text: line.to_string(),
            kind: TokenKind::Keyword,
        }];
    }

    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    for c in line.chars() {
        if c == '`' {
            if in_code {
                // Closing backtick belongs to the code span it closes.
                current.push('`');
                tokens.push(Token {
                    text: std::mem::take(&mut current),
                    kind: TokenKind::String,
                });
            } else {
                // Opening backtick starts the next (code) token.
                if !current.is_empty() {
                    tokens.push(Token {
                        text: std::mem::take(&mut current),
                        kind: TokenKind::Plain,
                    });
                }
                current.push('`');
            }
            in_code = !in_code;
            continue;
        }
        current.push(c);
    }
    if !current.is_empty() {
        tokens.push(Token {
            text: current,
            kind: if in_code {
                TokenKind::String
            } else {
                TokenKind::Plain
            },
        });
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str, language: Language) -> Vec<TokenKind> {
        highlight_line(line, language)
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    fn texts(line: &str, language: Language) -> Vec<String> {
        highlight_line(line, language)
            .into_iter()
            .map(|token| token.text)
            .collect()
    }

    #[test]
    fn language_is_detected_from_the_extension() {
        assert_eq!(Language::from_filename("main.py"), Language::Python);
        assert_eq!(Language::from_filename("board.h"), Language::C);
        assert_eq!(Language::from_filename("lib.rs"), Language::Rust);
        assert_eq!(Language::from_filename("Cargo.toml"), Language::Toml);
        assert_eq!(Language::from_filename("README"), Language::PlainText);
    }

    #[test]
    fn plain_text_is_a_single_unstyled_token() {
        let tokens = highlight_line("just some words", Language::PlainText);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Plain);
        assert_eq!(tokens[0].text, "just some words");
    }

    #[test]
    fn python_keywords_strings_and_comments_are_tagged() {
        let tokens = highlight_line("    return 'ok'  # done", Language::Python);
        assert!(
            tokens
                .iter()
                .any(|t| t.kind == TokenKind::Keyword && t.text == "return")
        );
        assert!(
            tokens
                .iter()
                .any(|t| t.kind == TokenKind::String && t.text == "'ok'")
        );
        assert!(
            tokens
                .iter()
                .any(|t| t.kind == TokenKind::Comment && t.text == "# done")
        );
    }

    #[test]
    fn a_comment_consumes_the_rest_of_the_line() {
        let tokens = highlight_line("x = 1 // trailing // not a new comment", Language::Rust);
        let comment = tokens
            .iter()
            .find(|t| t.kind == TokenKind::Comment)
            .unwrap();
        assert_eq!(comment.text, "// trailing // not a new comment");
    }

    #[test]
    fn numbers_are_tagged_including_hex_and_float_forms() {
        assert_eq!(kinds("42", Language::C), vec![TokenKind::Number]);
        assert_eq!(kinds("0x1F", Language::C), vec![TokenKind::Number]);
        assert_eq!(kinds("3.14", Language::C), vec![TokenKind::Number]);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string_early() {
        let tokens = highlight_line(r#"msg = "a \"quoted\" word""#, Language::Rust);
        let string = tokens.iter().find(|t| t.kind == TokenKind::String).unwrap();
        assert_eq!(string.text, r#""a \"quoted\" word""#);
    }

    #[test]
    fn an_unterminated_string_does_not_panic_and_consumes_to_the_end() {
        let tokens = highlight_line("x = \"never closed", Language::Python);
        let string = tokens.iter().find(|t| t.kind == TokenKind::String).unwrap();
        assert_eq!(string.text, "\"never closed");
    }

    #[test]
    fn json_recognises_its_own_literals_not_python_keywords() {
        let tokens = highlight_line("null", Language::Json);
        assert_eq!(tokens[0].kind, TokenKind::Keyword);
        let tokens = highlight_line("None", Language::Json);
        assert_eq!(
            tokens[0].kind,
            TokenKind::Plain,
            "Python's None is not JSON's"
        );
    }

    #[test]
    fn markdown_headings_are_a_single_styled_line() {
        let tokens = highlight_line("## Section title", Language::Markdown);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Keyword);
    }

    #[test]
    fn markdown_inline_code_is_tagged() {
        let texts = texts("run `mpremote devs` first", Language::Markdown);
        assert!(texts.contains(&"`mpremote devs`".to_string()));
    }

    #[test]
    fn non_ascii_content_does_not_panic() {
        // A byte-index scan would panic slicing mid-codepoint; this must not.
        let tokens = highlight_line("# configuração do dispositivo", Language::Python);
        assert_eq!(tokens[0].kind, TokenKind::Comment);
    }
}
