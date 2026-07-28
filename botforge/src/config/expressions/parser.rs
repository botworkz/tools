use anyhow::{Context, Result};

use super::lexer::{is_identifier_char, Lexer, Token};

/// AST node for the `${{ }}` expression language.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum ExprNode {
    String(String),
    Number(f64),
    Bool(bool),
    /// `namespace.name` reference, e.g. `inputs.flag` or `args.0`.
    Reference {
        namespace: String,
        name: String,
    },
    /// `func(arg)` function call, e.g. `to_json(x)` or `from_json(s)`.
    FunctionCall {
        name: String,
        arg: Box<ExprNode>,
    },
    Not(Box<ExprNode>),
    Equal(Box<ExprNode>, Box<ExprNode>),
    NotEqual(Box<ExprNode>, Box<ExprNode>),
    And(Box<ExprNode>, Box<ExprNode>),
    Or(Box<ExprNode>, Box<ExprNode>),
}

pub(super) struct Parser {
    lexer: Lexer,
    current: Token,
}

impl Parser {
    pub(super) fn parse(input: &str) -> Result<ExprNode> {
        let mut parser = Self {
            lexer: Lexer::new(input),
            current: Token::End,
        };
        parser.current = parser.lexer.next_token()?;
        let expr = parser.parse_or_expression()?;
        if parser.current != Token::End {
            anyhow::bail!(
                "unexpected trailing token in expression: {:?}",
                parser.current
            );
        }
        Ok(expr)
    }

    fn parse_or_expression(&mut self) -> Result<ExprNode> {
        let mut node = self.parse_and_expression()?;
        while self.current == Token::OrOr {
            self.advance()?;
            let rhs = self.parse_and_expression()?;
            node = ExprNode::Or(Box::new(node), Box::new(rhs));
        }
        Ok(node)
    }

    fn parse_and_expression(&mut self) -> Result<ExprNode> {
        let mut node = self.parse_equality_expression()?;
        while self.current == Token::AndAnd {
            self.advance()?;
            let rhs = self.parse_equality_expression()?;
            node = ExprNode::And(Box::new(node), Box::new(rhs));
        }
        Ok(node)
    }

    fn parse_equality_expression(&mut self) -> Result<ExprNode> {
        let mut node = self.parse_unary_expression()?;
        loop {
            let op = match self.current {
                Token::EqEq => Some(true),
                Token::NotEq => Some(false),
                _ => None,
            };
            let Some(is_eq) = op else { break };
            self.advance()?;
            let rhs = self.parse_unary_expression()?;
            node = if is_eq {
                ExprNode::Equal(Box::new(node), Box::new(rhs))
            } else {
                ExprNode::NotEqual(Box::new(node), Box::new(rhs))
            };
        }
        Ok(node)
    }

    fn parse_unary_expression(&mut self) -> Result<ExprNode> {
        if self.current == Token::Bang {
            self.advance()?;
            return Ok(ExprNode::Not(Box::new(self.parse_unary_expression()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<ExprNode> {
        match self.current.clone() {
            Token::StringLiteral(text) => {
                self.advance()?;
                Ok(ExprNode::String(text))
            }
            Token::NumberLiteral(number) => {
                let parsed = number
                    .parse::<f64>()
                    .with_context(|| format!("invalid number literal '{number}'"))?;
                self.advance()?;
                Ok(ExprNode::Number(parsed))
            }
            Token::Identifier(identifier) => {
                self.advance()?;
                match identifier.as_str() {
                    "true" => Ok(ExprNode::Bool(true)),
                    "false" => Ok(ExprNode::Bool(false)),
                    _ => {
                        // Check if this is a function call: IDENT ( expr )
                        if self.current == Token::LParen {
                            self.advance()?; // consume (
                            let arg = self.parse_or_expression()?;
                            self.expect_token(Token::RParen)?;
                            Ok(ExprNode::FunctionCall {
                                name: identifier,
                                arg: Box::new(arg),
                            })
                        } else {
                            // Namespace reference: namespace.name
                            self.expect_token(Token::Dot)?;
                            let name = match self.current.clone() {
                                Token::Identifier(name) => name,
                                Token::NumberLiteral(number) => number,
                                _ => {
                                    anyhow::bail!("expected reference name after '{}'.", identifier)
                                }
                            };
                            if !is_reference_name_valid(&name) {
                                anyhow::bail!("invalid input name '{name}'");
                            }
                            self.advance()?;
                            Ok(ExprNode::Reference {
                                namespace: identifier,
                                name,
                            })
                        }
                    }
                }
            }
            Token::LParen => {
                self.advance()?;
                let expr = self.parse_or_expression()?;
                self.expect_token(Token::RParen)?;
                Ok(expr)
            }
            other => anyhow::bail!("unexpected token in expression: {:?}", other),
        }
    }

    fn expect_token(&mut self, expected: Token) -> Result<()> {
        if self.current != expected {
            anyhow::bail!("expected token {:?} but found {:?}", expected, self.current);
        }
        self.advance()
    }

    fn advance(&mut self) -> Result<()> {
        self.current = self.lexer.next_token()?;
        Ok(())
    }
}

fn is_reference_name_valid(name: &str) -> bool {
    !name.is_empty() && name.chars().all(is_identifier_char)
}
