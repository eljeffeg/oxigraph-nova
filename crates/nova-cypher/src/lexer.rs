//! Hand-rolled lexer for the openCypher.
//!
//! No external parsing dependency (no `winnow`/`nom`/`pest`) is used — this
//! mirrors `oxigraph-nova-shacl`'s zero-dependency-by-default precedent and
//! keeps this crate's dependency graph limited to `oxrdf`/`spargebra`/`anyhow`.
//!
//! Cypher keywords are case-insensitive (`MATCH`, `match`, `Match` all work);
//! this lexer does not special-case keywords at all — every bare word becomes
//! a [`Tok::Ident`], and the parser decides whether a given identifier
//! position expects a keyword by comparing an uppercased copy.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dot,
    DotDot, // ..
    Dash,   // -
    Star,   // *
    Plus,   // +
    Slash,  // /
    Lt,     // <
    Gt,     // >
    Eq,     // =
    Ne,     // <>
    Le,     // <=
    Ge,     // >=
    Eof,
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Ident(s) => write!(f, "identifier `{s}`"),
            Tok::Str(s) => write!(f, "string {s:?}"),
            Tok::Int(n) => write!(f, "integer {n}"),
            Tok::Float(n) => write!(f, "float {n}"),
            Tok::LParen => write!(f, "'('"),
            Tok::RParen => write!(f, "')'"),
            Tok::LBracket => write!(f, "'['"),
            Tok::RBracket => write!(f, "']'"),
            Tok::LBrace => write!(f, "'{{'"),
            Tok::RBrace => write!(f, "'}}'"),
            Tok::Colon => write!(f, "':'"),
            Tok::Comma => write!(f, "','"),
            Tok::Dot => write!(f, "'.'"),
            Tok::DotDot => write!(f, "'..'"),
            Tok::Dash => write!(f, "'-'"),
            Tok::Star => write!(f, "'*'"),
            Tok::Plus => write!(f, "'+'"),
            Tok::Slash => write!(f, "'/'"),
            Tok::Lt => write!(f, "'<'"),
            Tok::Gt => write!(f, "'>'"),
            Tok::Eq => write!(f, "'='"),
            Tok::Ne => write!(f, "'<>'"),
            Tok::Le => write!(f, "'<='"),
            Tok::Ge => write!(f, "'>='"),
            Tok::Eof => write!(f, "end of input"),
        }
    }
}

/// A token plus its byte offset in the source (for error messages).
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub tok: Tok,
    pub pos: usize,
}

/// Tokenizes a full Cypher query string. Never panics — all failures are
/// returned as an `Err(String)` describing the offending character/position.
pub fn lex(src: &str) -> Result<Vec<Spanned>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < chars.len() {
        let c = chars[i];

        // Whitespace
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // Line comment: // ...
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        let start = i;

        match c {
            '(' => {
                out.push(Spanned {
                    tok: Tok::LParen,
                    pos: start,
                });
                i += 1;
            }
            ')' => {
                out.push(Spanned {
                    tok: Tok::RParen,
                    pos: start,
                });
                i += 1;
            }
            '[' => {
                out.push(Spanned {
                    tok: Tok::LBracket,
                    pos: start,
                });
                i += 1;
            }
            ']' => {
                out.push(Spanned {
                    tok: Tok::RBracket,
                    pos: start,
                });
                i += 1;
            }
            '{' => {
                out.push(Spanned {
                    tok: Tok::LBrace,
                    pos: start,
                });
                i += 1;
            }
            '}' => {
                out.push(Spanned {
                    tok: Tok::RBrace,
                    pos: start,
                });
                i += 1;
            }
            ':' => {
                out.push(Spanned {
                    tok: Tok::Colon,
                    pos: start,
                });
                i += 1;
            }
            ',' => {
                out.push(Spanned {
                    tok: Tok::Comma,
                    pos: start,
                });
                i += 1;
            }
            '.' => {
                if chars.get(i + 1) == Some(&'.') {
                    out.push(Spanned {
                        tok: Tok::DotDot,
                        pos: start,
                    });
                    i += 2;
                } else {
                    out.push(Spanned {
                        tok: Tok::Dot,
                        pos: start,
                    });
                    i += 1;
                }
            }
            '-' => {
                out.push(Spanned {
                    tok: Tok::Dash,
                    pos: start,
                });
                i += 1;
            }
            '*' => {
                out.push(Spanned {
                    tok: Tok::Star,
                    pos: start,
                });
                i += 1;
            }
            '+' => {
                out.push(Spanned {
                    tok: Tok::Plus,
                    pos: start,
                });
                i += 1;
            }
            '/' => {
                out.push(Spanned {
                    tok: Tok::Slash,
                    pos: start,
                });
                i += 1;
            }
            '=' => {
                out.push(Spanned {
                    tok: Tok::Eq,
                    pos: start,
                });
                i += 1;
            }
            '<' => {
                if chars.get(i + 1) == Some(&'>') {
                    out.push(Spanned {
                        tok: Tok::Ne,
                        pos: start,
                    });
                    i += 2;
                } else if chars.get(i + 1) == Some(&'=') {
                    out.push(Spanned {
                        tok: Tok::Le,
                        pos: start,
                    });
                    i += 2;
                } else {
                    out.push(Spanned {
                        tok: Tok::Lt,
                        pos: start,
                    });
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Spanned {
                        tok: Tok::Ge,
                        pos: start,
                    });
                    i += 2;
                } else {
                    out.push(Spanned {
                        tok: Tok::Gt,
                        pos: start,
                    });
                    i += 1;
                }
            }
            '\'' | '"' => {
                let quote = c;
                i += 1;
                let mut s = String::new();
                loop {
                    match chars.get(i) {
                        None => {
                            return Err(format!(
                                "unterminated string literal starting at byte {start}"
                            ));
                        }
                        Some(&'\\') if chars.get(i + 1).is_some() => {
                            let esc = chars[i + 1];
                            s.push(match esc {
                                'n' => '\n',
                                't' => '\t',
                                'r' => '\r',
                                '\\' => '\\',
                                '\'' => '\'',
                                '"' => '"',
                                other => other,
                            });
                            i += 2;
                        }
                        Some(&ch) if ch == quote => {
                            i += 1;
                            break;
                        }
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                out.push(Spanned {
                    tok: Tok::Str(s),
                    pos: start,
                });
            }
            c if c.is_ascii_digit() => {
                let mut s = String::new();
                let mut is_float = false;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    s.push(chars[i]);
                    i += 1;
                }
                if chars.get(i) == Some(&'.')
                    && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit())
                {
                    is_float = true;
                    s.push('.');
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        s.push(chars[i]);
                        i += 1;
                    }
                }
                if is_float {
                    let v: f64 = s
                        .parse()
                        .map_err(|_| format!("invalid float literal `{s}` at byte {start}"))?;
                    out.push(Spanned {
                        tok: Tok::Float(v),
                        pos: start,
                    });
                } else {
                    let v: i64 = s
                        .parse()
                        .map_err(|_| format!("invalid integer literal `{s}` at byte {start}"))?;
                    out.push(Spanned {
                        tok: Tok::Int(v),
                        pos: start,
                    });
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut s = String::new();
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    s.push(chars[i]);
                    i += 1;
                }
                out.push(Spanned {
                    tok: Tok::Ident(s),
                    pos: start,
                });
            }
            other => {
                return Err(format!("unexpected character `{other}` at byte {start}"));
            }
        }
    }

    out.push(Spanned {
        tok: Tok::Eof,
        pos: chars.len(),
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|s| s.tok).collect()
    }

    #[test]
    fn lexes_simple_match() {
        assert_eq!(
            toks("MATCH (n:Person) RETURN n"),
            vec![
                Tok::Ident("MATCH".into()),
                Tok::LParen,
                Tok::Ident("n".into()),
                Tok::Colon,
                Tok::Ident("Person".into()),
                Tok::RParen,
                Tok::Ident("RETURN".into()),
                Tok::Ident("n".into()),
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn lexes_relationship_arrows() {
        assert_eq!(
            toks("-[r:KNOWS]->"),
            vec![
                Tok::Dash,
                Tok::LBracket,
                Tok::Ident("r".into()),
                Tok::Colon,
                Tok::Ident("KNOWS".into()),
                Tok::RBracket,
                Tok::Dash,
                Tok::Gt,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn lexes_string_and_number_literals() {
        assert_eq!(
            toks("'hello' 42 3.5 <> <= >="),
            vec![
                Tok::Str("hello".into()),
                Tok::Int(42),
                Tok::Float(3.5),
                Tok::Ne,
                Tok::Le,
                Tok::Ge,
                Tok::Eof,
            ]
        );
    }

    #[test]
    fn errors_on_unterminated_string() {
        assert!(lex("'unterminated").is_err());
    }

    #[test]
    fn skips_line_comments() {
        assert_eq!(
            toks("MATCH // a comment\n(n)"),
            vec![
                Tok::Ident("MATCH".into()),
                Tok::LParen,
                Tok::Ident("n".into()),
                Tok::RParen,
                Tok::Eof,
            ]
        );
    }
}
