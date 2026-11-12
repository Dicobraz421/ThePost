//! Лексер Sieve (RFC 5228 §2, §8.1): идентификаторы, теги, числа с `K`/`M`/`G`, строки в кавычках, многострочные
//! `text:`, скобки и комментарии `#` и `/* */`. У каждого токена — позиция.

use crate::{Diagnostic, Pos};

/// Вид токена. Идентификаторы и теги приведены к нижнему регистру (RFC 5228 §2.3: регистр не важен).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tok {
    Ident(String),
    Tag(String),
    Num(u64),
    Str(String),
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    Semi,
}

/// Токен с позицией начала.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub tok: Tok,
    pub pos: Pos,
}

/// Разбивает текст на токены; возвращает и позицию конца текста (для ошибок «неожиданный конец»).
pub(crate) fn lex(src: &str) -> Result<(Vec<Token>, Pos), Diagnostic> {
    let mut lx = Lexer {
        chars: src.chars().collect(),
        i: 0,
        pos: Pos::START,
    };
    let mut out = Vec::new();
    while let Some(c) = lx.peek() {
        let pos = lx.pos;
        let single = match c {
            '[' => Some(Tok::LBracket),
            ']' => Some(Tok::RBracket),
            '{' => Some(Tok::LBrace),
            '}' => Some(Tok::RBrace),
            '(' => Some(Tok::LParen),
            ')' => Some(Tok::RParen),
            ',' => Some(Tok::Comma),
            ';' => Some(Tok::Semi),
            _ => None,
        };
        if let Some(tok) = single {
            lx.bump();
            out.push(Token { tok, pos });
            continue;
        }
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                lx.bump();
            }
            '#' => lx.line_comment(),
            '/' if lx.peek_at(1) == Some('*') => lx.block_comment(pos)?,
            '"' => {
                let s = lx.quoted(pos)?;
                out.push(Token {
                    tok: Tok::Str(s),
                    pos,
                });
            }
            ':' => {
                lx.bump();
                if !lx.peek().is_some_and(ident_start) {
                    return Err(Diagnostic::new(pos, "после «:» ожидается имя тега"));
                }
                let name = lx.ident();
                out.push(Token {
                    tok: Tok::Tag(name),
                    pos,
                });
            }
            c if c.is_ascii_digit() => {
                let n = lx.number(pos)?;
                out.push(Token {
                    tok: Tok::Num(n),
                    pos,
                });
            }
            c if ident_start(c) => {
                let name = lx.ident();
                let tok = if name == "text" && lx.peek() == Some(':') {
                    lx.bump();
                    Tok::Str(lx.multiline(pos)?)
                } else {
                    Tok::Ident(name)
                };
                out.push(Token { tok, pos });
            }
            other => {
                return Err(Diagnostic::new(
                    pos,
                    format!("неожиданный символ {other:?}"),
                ));
            }
        }
    }
    Ok((out, lx.pos))
}

fn ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    pos: Pos,
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn peek_at(&self, k: usize) -> Option<char> {
        self.chars.get(self.i.saturating_add(k)).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        if c == '\n' {
            self.pos.line = self.pos.line.saturating_add(1);
            self.pos.column = 1;
        } else {
            self.pos.column = self.pos.column.saturating_add(1);
        }
        Some(c)
    }

    fn ident(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek().filter(|c| ident_char(*c)) {
            self.bump();
            s.push(c.to_ascii_lowercase());
        }
        s
    }

    /// `#` до конца строки (перевод строки не съедается).
    fn line_comment(&mut self) {
        while self.peek().is_some_and(|c| c != '\n') {
            self.bump();
        }
    }

    fn block_comment(&mut self, start: Pos) -> Result<(), Diagnostic> {
        self.bump();
        self.bump();
        loop {
            match self.bump() {
                None => return Err(Diagnostic::new(start, "комментарий «/*» не закрыт")),
                Some('*') if self.peek() == Some('/') => {
                    self.bump();
                    return Ok(());
                }
                Some(_) => {}
            }
        }
    }

    /// Число с необязательным множителем `K`, `M`, `G` (RFC 5228 §2.4.1).
    fn number(&mut self, start: Pos) -> Result<u64, Diagnostic> {
        let too_big = || Diagnostic::new(start, "слишком большое число");
        let mut n: u64 = 0;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
            self.bump();
            n = n
                .checked_mul(10)
                .and_then(|n| n.checked_add(u64::from(d)))
                .ok_or_else(too_big)?;
        }
        let mul: u64 = match self.peek() {
            Some('k' | 'K') => 1 << 10,
            Some('m' | 'M') => 1 << 20,
            Some('g' | 'G') => 1 << 30,
            _ => 1,
        };
        if mul != 1 {
            self.bump();
            n = n.checked_mul(mul).ok_or_else(too_big)?;
        }
        Ok(n)
    }

    /// Строка в кавычках: `\\` и `\"` — экранирование; другой символ после `\` — как есть (RFC 5228 §2.4.2).
    fn quoted(&mut self, start: Pos) -> Result<String, Diagnostic> {
        self.bump();
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(Diagnostic::new(start, "строка не закрыта кавычкой")),
                Some('"') => return Ok(s),
                Some('\\') => match self.bump() {
                    Some(c) => s.push(c),
                    None => return Err(Diagnostic::new(start, "строка не закрыта кавычкой")),
                },
                Some(c) => s.push(c),
            }
        }
    }

    /// Многострочная строка после `text:`: до строки из одной точки; ведущая точка строки снимается
    /// (удвоение точки, RFC 5228 §2.4.2). Переводы строк сохраняются как в тексте.
    fn multiline(&mut self, start: Pos) -> Result<String, Diagnostic> {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
        if self.peek() == Some('#') {
            self.line_comment();
        }
        if self.peek() == Some('\r') {
            self.bump();
        }
        if self.bump() != Some('\n') {
            return Err(Diagnostic::new(
                start,
                "после «text:» ожидается перевод строки",
            ));
        }
        let mut s = String::new();
        loop {
            if self.peek().is_none() {
                return Err(Diagnostic::new(
                    start,
                    "многострочная строка не закрыта строкой «.»",
                ));
            }
            let mut line = String::new();
            while let Some(c) = self.bump() {
                line.push(c);
                if c == '\n' {
                    break;
                }
            }
            let body = line.strip_suffix('\n').unwrap_or(line.as_str());
            let body = body.strip_suffix('\r').unwrap_or(body);
            if body == "." {
                return Ok(s);
            }
            s.push_str(line.strip_prefix('.').unwrap_or(line.as_str()));
        }
    }
}
