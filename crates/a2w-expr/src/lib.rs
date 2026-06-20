//! # a2w-expr
//!
//! A tiny, safe expression DSL evaluated against a JSON item context. Used by
//! workflow params (Transform, Branch, Switch, HttpRequest body templating)
//! to express computed values without an embedded scripting language.
//!
//! ## Grammar
//!
//! ```text
//! expr     := or_expr
//! or_expr  := and_expr ( "||" and_expr )*
//! and_expr := not_expr ( "&&" not_expr )*
//! not_expr := "!" not_expr | cmp_expr
//! cmp_expr := add_expr ( ("==" | "!=" | "<" | ">" | "<=" | ">=") add_expr )?
//! add_expr := mul_expr ( ("+" | "-") mul_expr )*
//! mul_expr := unary    ( ("*" | "/" | "%") unary )*
//! unary    := ("-" | "+") atom | atom
//! atom     := literal | path | call | "(" expr ")"
//! literal  := number | string | "true" | "false" | "null"
//! path     := "$" ( "." ident | "[" (number|string) "]" )*
//! call     := ident "(" ( expr ("," expr)* )? ")"
//! ident    := [A-Za-z_][A-Za-z0-9_]*
//! ```
//!
//! ## Built-in functions
//!
//! | Function           | Description |
//! |--------------------|-------------|
//! | `length(v)`        | length of string / array / object |
//! | `contains(haystack, needle)` | substring (strings) or membership (arrays) |
//! | `upper(s)` / `lower(s)` | ASCII case conversion |
//! | `coalesce(a,b,...)` | first non-null argument |
//! | `if(cond, a, b)`   | ternary |
//! | `to_string(v)`     | JSON-stringify |
//! | `to_number(v)`     | parse string → number; identity for number |
//! | `not(v)`           | boolean NOT (synonym of `!`) |
//!
//! ## Use from a workflow
//!
//! Wrap an expression in `${{ ... }}` inside a string-typed param. For
//! example, in a `Transform.set`:
//! ```json
//! { "set": { "greeting": "${{ \"Hello, \" + $.name }}" } }
//! ```
//! Plain strings without the `${{ }}` markers pass through unchanged so
//! existing `{{json.path}}` templating still works.

#![forbid(unsafe_code)]

use serde_json::Value;
use thiserror::Error;

/// All errors the expression engine can surface.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExprError {
    /// Parser couldn't make sense of the input.
    #[error("parse error at offset {offset}: {message}")]
    Parse {
        /// Byte offset into the source where parsing failed.
        offset: usize,
        /// Human-readable description.
        message: String,
    },
    /// Evaluator ran into a runtime mismatch.
    #[error("eval error: {0}")]
    Eval(String),
}

/// Maximum recursive descent depth before [`ExprError::Parse`] (R3 audit-fix:
/// defends against stack-overflow DoS via `((((...))))`-style input).
const MAX_PARSE_DEPTH: usize = 64;
/// Maximum length of a single string literal in source (R3 audit-fix:
/// defends against memory-exhaustion DoS via an attacker-controlled
/// expression carrying a multi-MB `"…"`).
const MAX_STRING_LITERAL_BYTES: usize = 64 * 1024;
/// Maximum number of tokens an expression may produce (R5 audit-fix:
/// defends against pre-parser allocation DoS via unbounded `Vec<Token>`
/// from inputs like 100k `!` characters that depth-check only catches
/// AFTER tokenization).
const MAX_TOKENS: usize = 4096;
/// Maximum source length [`render`] will accept (R6 audit-fix: the
/// outer copy is byte-by-byte to a String, so a 100 MiB template with a
/// single `${{...}}` would still allocate 100 MiB outside the parser).
const MAX_RENDER_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    // Literals
    Number(f64),
    Str(String),
    True,
    False,
    Null,
    // Identifiers (for function calls / `not` etc.)
    Ident(String),
    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AndAnd,
    OrOr,
    Bang,
    // Punctuation
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Dollar,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self { src: src.as_bytes(), pos: 0 }
    }
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }
    fn advance(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }
    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
    fn next_token(&mut self) -> Result<Option<(Token, usize)>, ExprError> {
        self.skip_ws();
        let start = self.pos;
        let b = match self.peek() {
            Some(b) => b,
            None => return Ok(None),
        };
        // Multi-char operators first.
        let two = self.src.get(self.pos..self.pos + 2);
        if let Some(t) = two {
            match t {
                b"==" => {
                    self.pos += 2;
                    return Ok(Some((Token::EqEq, start)));
                }
                b"!=" => {
                    self.pos += 2;
                    return Ok(Some((Token::NotEq, start)));
                }
                b"<=" => {
                    self.pos += 2;
                    return Ok(Some((Token::LtEq, start)));
                }
                b">=" => {
                    self.pos += 2;
                    return Ok(Some((Token::GtEq, start)));
                }
                b"&&" => {
                    self.pos += 2;
                    return Ok(Some((Token::AndAnd, start)));
                }
                b"||" => {
                    self.pos += 2;
                    return Ok(Some((Token::OrOr, start)));
                }
                _ => {}
            }
        }
        match b {
            b'+' => {
                self.advance();
                Ok(Some((Token::Plus, start)))
            }
            b'-' => {
                self.advance();
                Ok(Some((Token::Minus, start)))
            }
            b'*' => {
                self.advance();
                Ok(Some((Token::Star, start)))
            }
            b'/' => {
                self.advance();
                Ok(Some((Token::Slash, start)))
            }
            b'%' => {
                self.advance();
                Ok(Some((Token::Percent, start)))
            }
            b'<' => {
                self.advance();
                Ok(Some((Token::Lt, start)))
            }
            b'>' => {
                self.advance();
                Ok(Some((Token::Gt, start)))
            }
            b'!' => {
                self.advance();
                Ok(Some((Token::Bang, start)))
            }
            b'(' => {
                self.advance();
                Ok(Some((Token::LParen, start)))
            }
            b')' => {
                self.advance();
                Ok(Some((Token::RParen, start)))
            }
            b'[' => {
                self.advance();
                Ok(Some((Token::LBracket, start)))
            }
            b']' => {
                self.advance();
                Ok(Some((Token::RBracket, start)))
            }
            b',' => {
                self.advance();
                Ok(Some((Token::Comma, start)))
            }
            b'.' => {
                self.advance();
                Ok(Some((Token::Dot, start)))
            }
            b'$' => {
                self.advance();
                Ok(Some((Token::Dollar, start)))
            }
            b'"' | b'\'' => {
                let quote = self.advance().unwrap();
                let mut s = String::new();
                while let Some(b) = self.peek() {
                    if b == quote {
                        self.advance();
                        return Ok(Some((Token::Str(s), start)));
                    }
                    if s.len() >= MAX_STRING_LITERAL_BYTES {
                        return Err(ExprError::Parse {
                            offset: start,
                            message: format!(
                                "string literal exceeds {MAX_STRING_LITERAL_BYTES}-byte cap"
                            ),
                        });
                    }
                    if b == b'\\' {
                        self.advance();
                        match self.advance() {
                            Some(b'n') => s.push('\n'),
                            Some(b't') => s.push('\t'),
                            Some(b'\\') => s.push('\\'),
                            Some(b'"') => s.push('"'),
                            Some(b'\'') => s.push('\''),
                            Some(other) => s.push(other as char),
                            None => {
                                return Err(ExprError::Parse {
                                    offset: self.pos,
                                    message: "unterminated escape in string".into(),
                                });
                            }
                        }
                    } else {
                        s.push(self.advance().unwrap() as char);
                    }
                }
                Err(ExprError::Parse {
                    offset: start,
                    message: "unterminated string literal".into(),
                })
            }
            b'0'..=b'9' => {
                let mut s = String::new();
                while let Some(b) = self.peek() {
                    if b.is_ascii_digit() || b == b'.' || b == b'e' || b == b'E' || b == b'-'
                        || b == b'+'
                    {
                        // Stop if minus/plus follows a non-digit/e (so 1-2 isn't read as 1, -2).
                        if (b == b'-' || b == b'+')
                            && !matches!(
                                s.bytes().last(),
                                Some(b'e') | Some(b'E')
                            )
                        {
                            break;
                        }
                        s.push(self.advance().unwrap() as char);
                    } else {
                        break;
                    }
                }
                let n: f64 = s.parse().map_err(|e| ExprError::Parse {
                    offset: start,
                    message: format!("bad number literal '{s}': {e}"),
                })?;
                // R3 audit-fix: reject overflow-to-infinity and NaN so a
                // workflow can't accidentally smuggle inf/-inf into the
                // evaluator (downstream JSON serialization would render them
                // as `null`, hiding the bug).
                if !n.is_finite() {
                    return Err(ExprError::Parse {
                        offset: start,
                        message: format!(
                            "number literal '{s}' is not finite (overflow / NaN)"
                        ),
                    });
                }
                Ok(Some((Token::Number(n), start)))
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let mut s = String::new();
                while let Some(b) = self.peek() {
                    if b.is_ascii_alphanumeric() || b == b'_' {
                        s.push(self.advance().unwrap() as char);
                    } else {
                        break;
                    }
                }
                let tok = match s.as_str() {
                    "true" => Token::True,
                    "false" => Token::False,
                    "null" => Token::Null,
                    _ => Token::Ident(s),
                };
                Ok(Some((tok, start)))
            }
            other => Err(ExprError::Parse {
                offset: start,
                message: format!("unexpected character '{}'", other as char),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Lit(Value),
    Path(Vec<PathStep>),
    Call(String, Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum PathStep {
    Field(String),
    Index(usize),
    Key(String),
}

#[derive(Debug, Clone, PartialEq, Copy)]
enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, PartialEq, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<(Token, usize)>,
    pos: usize,
    /// Current recursive-descent depth — R3 audit-fix cap. Incremented at
    /// each `parse_or` entry (which all alternatives funnel into via parens
    /// and function-call args) and decremented on return.
    depth: usize,
}

impl Parser {
    fn new(src: &str) -> Result<Self, ExprError> {
        let mut lex = Lexer::new(src);
        let mut tokens = Vec::new();
        while let Some((tok, offset)) = lex.next_token()? {
            if tokens.len() >= MAX_TOKENS {
                return Err(ExprError::Parse {
                    offset,
                    message: format!(
                        "expression exceeds {MAX_TOKENS}-token cap (pre-parser \
                         memory amplification guard)"
                    ),
                });
            }
            tokens.push((tok, offset));
        }
        Ok(Self {
            tokens,
            pos: 0,
            depth: 0,
        })
    }

    fn descend(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(ExprError::Parse {
                offset: self.offset(),
                message: format!(
                    "expression nests deeper than {MAX_PARSE_DEPTH} levels — \
                     refusing to parse"
                ),
            });
        }
        Ok(())
    }
    fn ascend(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }
    fn consume(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).map(|(t, _)| t.clone())?;
        self.pos += 1;
        Some(t)
    }
    fn offset(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map(|(_, o)| *o)
            .unwrap_or_default()
    }
    fn parse_top(&mut self) -> Result<Expr, ExprError> {
        let e = self.parse_or()?;
        if self.pos < self.tokens.len() {
            return Err(ExprError::Parse {
                offset: self.offset(),
                message: format!("trailing tokens after expression: {:?}", self.peek()),
            });
        }
        Ok(e)
    }
    fn parse_or(&mut self) -> Result<Expr, ExprError> {
        self.descend()?;
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Some(Token::OrOr)) {
            self.consume();
            let rhs = self.parse_and()?;
            lhs = Expr::Binary(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        self.ascend();
        Ok(lhs)
    }
    fn parse_and(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_not()?;
        while matches!(self.peek(), Some(Token::AndAnd)) {
            self.consume();
            let rhs = self.parse_not()?;
            lhs = Expr::Binary(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_not(&mut self) -> Result<Expr, ExprError> {
        if matches!(self.peek(), Some(Token::Bang)) {
            self.descend()?;
            self.consume();
            let inner = self.parse_not()?;
            self.ascend();
            return Ok(Expr::Unary(UnOp::Not, Box::new(inner)));
        }
        self.parse_cmp()
    }
    fn parse_cmp(&mut self) -> Result<Expr, ExprError> {
        let lhs = self.parse_add()?;
        let op = match self.peek() {
            Some(Token::EqEq) => Some(BinOp::Eq),
            Some(Token::NotEq) => Some(BinOp::Ne),
            Some(Token::Lt) => Some(BinOp::Lt),
            Some(Token::Gt) => Some(BinOp::Gt),
            Some(Token::LtEq) => Some(BinOp::Le),
            Some(Token::GtEq) => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.consume();
            let rhs = self.parse_add()?;
            return Ok(Expr::Binary(op, Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }
    fn parse_add(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinOp::Add,
                Some(Token::Minus) => BinOp::Sub,
                _ => break,
            };
            self.consume();
            let rhs = self.parse_mul()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_mul(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinOp::Mul,
                Some(Token::Slash) => BinOp::Div,
                Some(Token::Percent) => BinOp::Mod,
                _ => break,
            };
            self.consume();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_unary(&mut self) -> Result<Expr, ExprError> {
        match self.peek() {
            Some(Token::Minus) => {
                self.descend()?;
                self.consume();
                let inner = self.parse_unary()?;
                self.ascend();
                Ok(Expr::Unary(UnOp::Neg, Box::new(inner)))
            }
            Some(Token::Plus) => {
                self.descend()?;
                self.consume();
                let inner = self.parse_unary()?;
                self.ascend();
                Ok(inner)
            }
            _ => self.parse_atom(),
        }
    }
    fn parse_atom(&mut self) -> Result<Expr, ExprError> {
        match self.consume() {
            Some(Token::Number(n)) => Ok(Expr::Lit(Value::from(n))),
            Some(Token::Str(s)) => Ok(Expr::Lit(Value::String(s))),
            Some(Token::True) => Ok(Expr::Lit(Value::Bool(true))),
            Some(Token::False) => Ok(Expr::Lit(Value::Bool(false))),
            Some(Token::Null) => Ok(Expr::Lit(Value::Null)),
            Some(Token::Dollar) => self.parse_path(),
            Some(Token::Ident(name)) => {
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.consume();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Token::RParen)) {
                        loop {
                            args.push(self.parse_or()?);
                            match self.peek() {
                                Some(Token::Comma) => {
                                    self.consume();
                                }
                                _ => break,
                            }
                        }
                    }
                    match self.consume() {
                        Some(Token::RParen) => {}
                        other => {
                            return Err(ExprError::Parse {
                                offset: self.offset(),
                                message: format!(
                                    "expected ')' to close call to '{name}', got {other:?}"
                                ),
                            });
                        }
                    }
                    Ok(Expr::Call(name, args))
                } else {
                    Err(ExprError::Parse {
                        offset: self.offset(),
                        message: format!("bare identifier '{name}' not allowed (use $ for paths)"),
                    })
                }
            }
            Some(Token::LParen) => {
                // parse_or() itself calls descend(), so we don't double-count
                // here. The paren-recursion is already covered.
                let e = self.parse_or()?;
                match self.consume() {
                    Some(Token::RParen) => Ok(e),
                    other => Err(ExprError::Parse {
                        offset: self.offset(),
                        message: format!("expected ')', got {other:?}"),
                    }),
                }
            }
            other => Err(ExprError::Parse {
                offset: self.offset(),
                message: format!("unexpected token {other:?}"),
            }),
        }
    }
    fn parse_path(&mut self) -> Result<Expr, ExprError> {
        let mut steps = Vec::new();
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.consume();
                    match self.consume() {
                        Some(Token::Ident(name)) => steps.push(PathStep::Field(name)),
                        other => {
                            return Err(ExprError::Parse {
                                offset: self.offset(),
                                message: format!("expected identifier after '.', got {other:?}"),
                            });
                        }
                    }
                }
                Some(Token::LBracket) => {
                    self.consume();
                    match self.consume() {
                        Some(Token::Number(n)) => {
                            let i = n as usize;
                            steps.push(PathStep::Index(i));
                        }
                        Some(Token::Str(s)) => steps.push(PathStep::Key(s)),
                        other => {
                            return Err(ExprError::Parse {
                                offset: self.offset(),
                                message: format!(
                                    "expected number or string in '[..]', got {other:?}"
                                ),
                            });
                        }
                    }
                    match self.consume() {
                        Some(Token::RBracket) => {}
                        other => {
                            return Err(ExprError::Parse {
                                offset: self.offset(),
                                message: format!("expected ']', got {other:?}"),
                            });
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(Expr::Path(steps))
    }
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

fn lookup<'a>(item: &'a Value, steps: &[PathStep]) -> Option<&'a Value> {
    let mut cur = item;
    for s in steps {
        match (cur, s) {
            (Value::Object(m), PathStep::Field(f)) => cur = m.get(f)?,
            (Value::Object(m), PathStep::Key(k)) => cur = m.get(k)?,
            (Value::Array(a), PathStep::Index(i)) => cur = a.get(*i)?,
            (Value::Array(a), PathStep::Key(k)) => cur = a.get(k.parse::<usize>().ok()?)?,
            _ => return None,
        }
    }
    Some(cur)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn as_number(v: &Value) -> Result<f64, ExprError> {
    match v {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| ExprError::Eval(format!("number '{n}' is not f64-representable"))),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Null => Ok(0.0),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| ExprError::Eval(format!("cannot coerce string '{s}' to number"))),
        _ => Err(ExprError::Eval(format!(
            "cannot coerce {v} to number"
        ))),
    }
}

fn eval(e: &Expr, item: &Value) -> Result<Value, ExprError> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Path(steps) => lookup(item, steps).cloned().unwrap_or(Value::Null),
        Expr::Unary(op, inner) => {
            let v = eval(inner, item)?;
            match op {
                UnOp::Neg => Value::from(-as_number(&v)?),
                UnOp::Not => Value::Bool(!truthy(&v)),
            }
        }
        Expr::Binary(op, l, r) => {
            // Short-circuit && and ||.
            if matches!(op, BinOp::And) {
                let lv = eval(l, item)?;
                if !truthy(&lv) {
                    return Ok(lv);
                }
                return eval(r, item);
            }
            if matches!(op, BinOp::Or) {
                let lv = eval(l, item)?;
                if truthy(&lv) {
                    return Ok(lv);
                }
                return eval(r, item);
            }
            let lv = eval(l, item)?;
            let rv = eval(r, item)?;
            match op {
                BinOp::Eq => Value::Bool(lv == rv),
                BinOp::Ne => Value::Bool(lv != rv),
                BinOp::Lt => Value::Bool(as_number(&lv)? < as_number(&rv)?),
                BinOp::Gt => Value::Bool(as_number(&lv)? > as_number(&rv)?),
                BinOp::Le => Value::Bool(as_number(&lv)? <= as_number(&rv)?),
                BinOp::Ge => Value::Bool(as_number(&lv)? >= as_number(&rv)?),
                BinOp::Add => {
                    // String concat if either is string.
                    if matches!(&lv, Value::String(_)) || matches!(&rv, Value::String(_)) {
                        let ls = match &lv {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let rs = match &rv {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        Value::String(format!("{ls}{rs}"))
                    } else {
                        Value::from(as_number(&lv)? + as_number(&rv)?)
                    }
                }
                BinOp::Sub => Value::from(as_number(&lv)? - as_number(&rv)?),
                BinOp::Mul => Value::from(as_number(&lv)? * as_number(&rv)?),
                BinOp::Div => {
                    let r = as_number(&rv)?;
                    if r == 0.0 {
                        return Err(ExprError::Eval("division by zero".into()));
                    }
                    Value::from(as_number(&lv)? / r)
                }
                BinOp::Mod => {
                    let r = as_number(&rv)?;
                    if r == 0.0 {
                        return Err(ExprError::Eval("modulo by zero".into()));
                    }
                    Value::from(as_number(&lv)? % r)
                }
                BinOp::And | BinOp::Or => unreachable!("handled above"),
            }
        }
        Expr::Call(name, args) => call_builtin(name, args, item)?,
    })
}

fn call_builtin(name: &str, args: &[Expr], item: &Value) -> Result<Value, ExprError> {
    let eval_args = |a: &[Expr]| -> Result<Vec<Value>, ExprError> {
        a.iter().map(|e| eval(e, item)).collect()
    };
    match name {
        "length" => {
            let argv = eval_args(args)?;
            if argv.len() != 1 {
                return Err(ExprError::Eval("length() takes 1 arg".into()));
            }
            Ok(Value::from(match &argv[0] {
                Value::String(s) => s.chars().count() as i64,
                Value::Array(a) => a.len() as i64,
                Value::Object(o) => o.len() as i64,
                _ => 0,
            }))
        }
        "contains" => {
            let argv = eval_args(args)?;
            if argv.len() != 2 {
                return Err(ExprError::Eval("contains() takes 2 args".into()));
            }
            Ok(Value::Bool(match (&argv[0], &argv[1]) {
                (Value::String(h), Value::String(n)) => h.contains(n.as_str()),
                (Value::Array(a), needle) => a.iter().any(|v| v == needle),
                (Value::Object(o), Value::String(k)) => o.contains_key(k.as_str()),
                _ => false,
            }))
        }
        "upper" | "lower" => {
            let argv = eval_args(args)?;
            if argv.len() != 1 {
                return Err(ExprError::Eval(format!("{name}() takes 1 arg")));
            }
            let s = match &argv[0] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Ok(Value::String(if name == "upper" {
                s.to_ascii_uppercase()
            } else {
                s.to_ascii_lowercase()
            }))
        }
        "coalesce" => {
            for a in args {
                let v = eval(a, item)?;
                if !matches!(v, Value::Null) {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        "if" => {
            if args.len() != 3 {
                return Err(ExprError::Eval("if(cond, a, b) takes 3 args".into()));
            }
            let cond = eval(&args[0], item)?;
            if truthy(&cond) {
                eval(&args[1], item)
            } else {
                eval(&args[2], item)
            }
        }
        "to_string" => {
            let argv = eval_args(args)?;
            if argv.len() != 1 {
                return Err(ExprError::Eval("to_string() takes 1 arg".into()));
            }
            Ok(Value::String(match &argv[0] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }))
        }
        "to_number" => {
            let argv = eval_args(args)?;
            if argv.len() != 1 {
                return Err(ExprError::Eval("to_number() takes 1 arg".into()));
            }
            Ok(Value::from(as_number(&argv[0])?))
        }
        "not" => {
            let argv = eval_args(args)?;
            if argv.len() != 1 {
                return Err(ExprError::Eval("not() takes 1 arg".into()));
            }
            Ok(Value::Bool(!truthy(&argv[0])))
        }
        other => Err(ExprError::Eval(format!("unknown function '{other}'"))),
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse and evaluate `src` against `item`.
///
/// # Errors
/// Returns [`ExprError::Parse`] for syntax errors, [`ExprError::Eval`] for
/// type / range failures at evaluation.
pub fn eval_str(src: &str, item: &Value) -> Result<Value, ExprError> {
    let ast = Parser::new(src)?.parse_top()?;
    eval(&ast, item)
}

/// Render a templated string: any substring delimited by `${{` and `}}` is
/// parsed as an expression and replaced by its stringified evaluation. Outer
/// text passes through unchanged. Nested `${{` is not supported.
///
/// Plain `{{json.path}}` (the legacy templating) is NOT touched by this
/// function — callers compose the two pipelines explicitly.
#[must_use]
pub fn render(src: &str, item: &Value) -> String {
    // R6 audit-fix: cap the source length so a multi-MB template doesn't
    // amplify into a multi-MB output allocation. Excess is truncated with
    // a visible marker so the workflow author can see what happened.
    if src.len() > MAX_RENDER_BYTES {
        let mut out = String::with_capacity(MAX_RENDER_BYTES + 64);
        // Truncate on a char boundary.
        let cutoff = (0..=MAX_RENDER_BYTES)
            .rev()
            .find(|i| src.is_char_boundary(*i))
            .unwrap_or(0);
        out.push_str(&src[..cutoff]);
        out.push_str("[...render input truncated]");
        return out;
    }
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i..i + 3) == Some(b"${{") {
            // Find the matching `}}` (no nesting).
            let mut j = i + 3;
            while j + 1 < bytes.len() {
                if &bytes[j..j + 2] == b"}}" {
                    break;
                }
                j += 1;
            }
            if j + 1 >= bytes.len() {
                // No close — emit verbatim.
                out.push_str(&src[i..]);
                return out;
            }
            let expr = &src[i + 3..j];
            match eval_str(expr.trim(), item) {
                Ok(Value::String(s)) => out.push_str(&s),
                Ok(other) => out.push_str(&other.to_string()),
                // On error, emit the original delimited expression so the
                // workflow author can see what failed instead of a silent
                // empty replacement.
                Err(e) => out.push_str(&format!("${{{{!{e}!}}}}")),
            }
            i = j + 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item() -> Value {
        json!({
            "name": "Alice",
            "age": 30,
            "tags": ["admin", "active"],
            "address": { "city": "NYC", "zip": "10001" },
            "flag": true
        })
    }

    #[test]
    fn lit_eval() {
        assert_eq!(eval_str("42", &Value::Null).unwrap(), json!(42.0));
        assert_eq!(eval_str("\"hi\"", &Value::Null).unwrap(), json!("hi"));
        assert_eq!(eval_str("true", &Value::Null).unwrap(), json!(true));
        assert_eq!(eval_str("null", &Value::Null).unwrap(), json!(null));
    }

    #[test]
    fn path_lookup() {
        let v = item();
        assert_eq!(eval_str("$.name", &v).unwrap(), json!("Alice"));
        assert_eq!(eval_str("$.address.city", &v).unwrap(), json!("NYC"));
        assert_eq!(eval_str("$.tags[0]", &v).unwrap(), json!("admin"));
        assert_eq!(eval_str("$.missing", &v).unwrap(), json!(null));
    }

    #[test]
    fn arithmetic_and_concat() {
        let v = item();
        assert_eq!(eval_str("$.age + 5", &v).unwrap(), json!(35.0));
        assert_eq!(
            eval_str("\"Hello, \" + $.name", &v).unwrap(),
            json!("Hello, Alice")
        );
        assert_eq!(eval_str("10 / 4", &Value::Null).unwrap(), json!(2.5));
        assert_eq!(eval_str("10 % 3", &Value::Null).unwrap(), json!(1.0));
    }

    #[test]
    fn comparisons_and_logic() {
        let v = item();
        assert_eq!(eval_str("$.age > 18", &v).unwrap(), json!(true));
        assert_eq!(eval_str("$.flag && $.age < 100", &v).unwrap(), json!(true));
        assert_eq!(eval_str("$.flag || false", &v).unwrap(), json!(true));
        assert_eq!(eval_str("!$.flag", &v).unwrap(), json!(false));
        assert_eq!(eval_str("$.name == \"Alice\"", &v).unwrap(), json!(true));
    }

    #[test]
    fn builtins() {
        let v = item();
        assert_eq!(eval_str("length($.name)", &v).unwrap(), json!(5));
        assert_eq!(eval_str("length($.tags)", &v).unwrap(), json!(2));
        assert_eq!(
            eval_str("contains($.tags, \"admin\")", &v).unwrap(),
            json!(true)
        );
        assert_eq!(
            eval_str("upper($.name)", &v).unwrap(),
            json!("ALICE")
        );
        assert_eq!(
            eval_str("if($.age > 18, \"adult\", \"minor\")", &v).unwrap(),
            json!("adult")
        );
        assert_eq!(
            eval_str("coalesce($.missing, $.name)", &v).unwrap(),
            json!("Alice")
        );
    }

    #[test]
    fn render_templates_in_string() {
        let v = item();
        assert_eq!(
            render("Hi ${{ $.name }}, you have ${{ length($.tags) }} tags.", &v),
            "Hi Alice, you have 2 tags."
        );
    }

    #[test]
    fn render_passes_through_unmatched_braces() {
        let v = item();
        assert_eq!(render("no expression here", &v), "no expression here");
        assert_eq!(render("${{ unterminated", &v), "${{ unterminated");
    }

    #[test]
    fn parse_errors_caught_not_panic() {
        assert!(matches!(
            eval_str("$.+", &Value::Null),
            Err(ExprError::Parse { .. })
        ));
        assert!(matches!(
            eval_str("1 / 0", &Value::Null),
            Err(ExprError::Eval(_))
        ));
        assert!(matches!(
            eval_str("unknown_fn(1)", &Value::Null),
            Err(ExprError::Eval(_))
        ));
    }

    #[test]
    fn deep_nesting_does_not_panic() {
        // 50 levels of parens — under the cap, must succeed.
        let src = format!("{}{}{}", "(".repeat(50), "1+2", ")".repeat(50));
        assert_eq!(eval_str(&src, &Value::Null).unwrap(), json!(3.0));
    }

    #[test]
    fn excessive_depth_rejected_not_overflowed() {
        // 1000 levels of parens — must error cleanly via the parse-depth cap,
        // NOT overflow the stack.
        let src = format!("{}{}{}", "(".repeat(1000), "1", ")".repeat(1000));
        assert!(matches!(
            eval_str(&src, &Value::Null),
            Err(ExprError::Parse { .. })
        ));
    }

    #[test]
    fn excessive_unary_chain_rejected_not_overflowed() {
        // R4 audit-fix: unary `!`, `-`, `+` chains must also count against
        // the depth cap so an attacker can't DoS via `!!!!!!!!...1`.
        let neg = format!("{}1", "-".repeat(1000));
        assert!(matches!(
            eval_str(&neg, &Value::Null),
            Err(ExprError::Parse { .. })
        ));
        let not = format!("{}true", "!".repeat(1000));
        assert!(matches!(
            eval_str(&not, &Value::Null),
            Err(ExprError::Parse { .. })
        ));
        let plus = format!("{}1", "+".repeat(1000));
        assert!(matches!(
            eval_str(&plus, &Value::Null),
            Err(ExprError::Parse { .. })
        ));
    }

    #[test]
    fn string_literal_length_cap_enforced() {
        let huge = "x".repeat(200_000);
        let src = format!("\"{huge}\"");
        let err = eval_str(&src, &Value::Null).expect_err("must cap");
        assert!(matches!(err, ExprError::Parse { .. }));
        assert!(err.to_string().contains("cap"));
    }

    #[test]
    fn number_overflow_to_infinity_rejected() {
        let err = eval_str("1e9999", &Value::Null).expect_err("must reject inf");
        assert!(matches!(err, ExprError::Parse { .. }));
        assert!(err.to_string().contains("finite"));
    }
}
