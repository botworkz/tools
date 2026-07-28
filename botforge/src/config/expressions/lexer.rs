use anyhow::Result;

/// Tokens produced by the `${{ }}` expression lexer.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Token {
    OrOr,
    AndAnd,
    EqEq,
    NotEq,
    Bang,
    LParen,
    RParen,
    Dot,
    Comma,
    Identifier(String),
    StringLiteral(String),
    NumberLiteral(String),
    End,
}

pub(super) struct Lexer {
    chars: Vec<char>,
    pos: usize,
}

impl Lexer {
    pub(super) fn new(input: &str) -> Self {
        Self {
            chars: input.chars().collect(),
            pos: 0,
        }
    }

    pub(super) fn next_token(&mut self) -> Result<Token> {
        self.skip_whitespace();
        if self.pos >= self.chars.len() {
            return Ok(Token::End);
        }

        let ch = self.chars[self.pos];
        match ch {
            '|' => {
                if self.peek_char(1) == Some('|') {
                    self.pos += 2;
                    Ok(Token::OrOr)
                } else {
                    anyhow::bail!("unexpected '|' at position {}", self.pos);
                }
            }
            '&' => {
                if self.peek_char(1) == Some('&') {
                    self.pos += 2;
                    Ok(Token::AndAnd)
                } else {
                    anyhow::bail!("unexpected '&' at position {}", self.pos);
                }
            }
            '=' => {
                if self.peek_char(1) == Some('=') {
                    self.pos += 2;
                    Ok(Token::EqEq)
                } else {
                    anyhow::bail!("unexpected '=' at position {}; use '=='", self.pos);
                }
            }
            '!' => {
                if self.peek_char(1) == Some('=') {
                    self.pos += 2;
                    Ok(Token::NotEq)
                } else {
                    self.pos += 1;
                    Ok(Token::Bang)
                }
            }
            '(' => {
                self.pos += 1;
                Ok(Token::LParen)
            }
            ')' => {
                self.pos += 1;
                Ok(Token::RParen)
            }
            '.' => {
                self.pos += 1;
                Ok(Token::Dot)
            }
            ',' => {
                self.pos += 1;
                Ok(Token::Comma)
            }
            '\'' => self.read_single_quoted_string(),
            '0'..='9' => self.read_number(),
            _ if is_identifier_char(ch) => self.read_identifier(),
            _ => anyhow::bail!("unexpected character '{}' at position {}", ch, self.pos),
        }
    }

    fn read_single_quoted_string(&mut self) -> Result<Token> {
        self.pos += 1; // opening '
        let mut output = String::new();
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            self.pos += 1;
            match ch {
                '\'' => return Ok(Token::StringLiteral(output)),
                '\\' => {
                    let Some(next) = self.chars.get(self.pos).copied() else {
                        anyhow::bail!("unterminated escape in string literal");
                    };
                    self.pos += 1;
                    output.push(match next {
                        '\'' => '\'',
                        '\\' => '\\',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        other => other,
                    });
                }
                other => output.push(other),
            }
        }
        anyhow::bail!("unterminated string literal")
    }

    fn read_number(&mut self) -> Result<Token> {
        let start = self.pos;
        let mut seen_dot = false;
        while self.pos < self.chars.len() {
            match self.chars[self.pos] {
                '0'..='9' => self.pos += 1,
                '.' if !seen_dot => {
                    seen_dot = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let literal: String = self.chars[start..self.pos].iter().collect();
        Ok(Token::NumberLiteral(literal))
    }

    fn read_identifier(&mut self) -> Result<Token> {
        let start = self.pos;
        while self.pos < self.chars.len() && is_identifier_char(self.chars[self.pos]) {
            self.pos += 1;
        }
        let identifier: String = self.chars[start..self.pos].iter().collect();
        if identifier.is_empty() {
            anyhow::bail!("expected identifier at position {}", start);
        }
        Ok(Token::Identifier(identifier))
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek_char(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }
}

pub(super) fn is_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}
