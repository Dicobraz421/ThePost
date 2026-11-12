//! Синтаксическое дерево скрипта Sieve и его разбор (RFC 5228 §8.2, SIEVE-01).
//!
//! Разбор не знает команд и расширений — только грамматику; смысл проверяет [`crate::compile`]. Лимиты
//! ([`Limits::max_script_size`], [`Limits::max_nesting`], [`Limits::max_list_len`], [`Limits::max_commands`]) дают
//! ошибку разбора; глубина рекурсии ограничена `max_nesting`.

use std::fmt;
use std::iter::Peekable;
use std::vec::IntoIter;

use crate::lexer::{Tok, Token, lex};
use crate::{Diagnostic, Limits, Pos};

/// Разобранный скрипт.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// Команды верхнего уровня.
    pub commands: Vec<Command>,
}

/// Команда: `identifier arguments (";" / block)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Имя в нижнем регистре.
    pub name: String,
    /// Позиция имени.
    pub pos: Pos,
    /// Аргументы до теста.
    pub args: Vec<Arg>,
    /// Тест или список тестов.
    pub tests: Vec<Test>,
    /// Тесты заданы списком в скобках (`test-list`).
    pub test_list: bool,
    /// Блок `{…}`; `None` — команда закончилась `;`.
    pub block: Option<Vec<Command>>,
}

/// Тест: `identifier arguments`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Test {
    /// Имя в нижнем регистре.
    pub name: String,
    /// Позиция имени.
    pub pos: Pos,
    /// Аргументы до вложенного теста.
    pub args: Vec<Arg>,
    /// Вложенный тест или список тестов (`not`, `allof`, `anyof`).
    pub tests: Vec<Test>,
    /// Вложенные тесты заданы списком в скобках.
    pub test_list: bool,
}

/// Аргумент.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// Тег `:name` (имя в нижнем регистре, без двоеточия).
    Tag {
        /// Имя.
        name: String,
        /// Позиция.
        pos: Pos,
    },
    /// Число (множители `K`, `M`, `G` уже применены).
    Number {
        /// Значение.
        value: u64,
        /// Позиция.
        pos: Pos,
    },
    /// Строка или список строк.
    Strings {
        /// Значения.
        values: Vec<String>,
        /// Задано списком в квадратных скобках.
        list: bool,
        /// Позиция.
        pos: Pos,
    },
}

impl Arg {
    /// Позиция аргумента.
    pub fn pos(&self) -> Pos {
        match self {
            Arg::Tag { pos, .. } | Arg::Number { pos, .. } | Arg::Strings { pos, .. } => *pos,
        }
    }
}

/// Разбирает текст скрипта. Не UTF-8, превышение лимитов и синтаксические ошибки — [`Diagnostic`] с позицией.
pub fn parse(src: &[u8], limits: &Limits) -> Result<Script, Diagnostic> {
    if src.len() > limits.max_script_size {
        return Err(Diagnostic::new(
            Pos::START,
            format!("скрипт больше {} байт", limits.max_script_size),
        ));
    }
    let text = std::str::from_utf8(src).map_err(|e| {
        Diagnostic::new(
            pos_at(src.get(..e.valid_up_to()).unwrap_or_default()),
            "текст скрипта не в UTF-8",
        )
    })?;
    let (toks, end) = lex(text)?;
    let mut p = Parser {
        toks: toks.into_iter().peekable(),
        end,
        limits,
        nodes: 0,
    };
    let commands = p.commands(0)?;
    if let Some(t) = p.toks.next() {
        return Err(Diagnostic::new(t.pos, "лишняя «}»"));
    }
    Ok(Script { commands })
}

/// Позиция за концом корректного UTF-8 префикса.
fn pos_at(prefix: &[u8]) -> Pos {
    let mut pos = Pos::START;
    for c in String::from_utf8_lossy(prefix).chars() {
        if c == '\n' {
            pos.line = pos.line.saturating_add(1);
            pos.column = 1;
        } else {
            pos.column = pos.column.saturating_add(1);
        }
    }
    pos
}

struct Parser<'l> {
    toks: Peekable<IntoIter<Token>>,
    end: Pos,
    limits: &'l Limits,
    nodes: usize,
}

impl Parser<'_> {
    fn next(&mut self, what: &str) -> Result<Token, Diagnostic> {
        self.toks
            .next()
            .ok_or_else(|| Diagnostic::new(self.end, format!("неожиданный конец скрипта: {what}")))
    }

    fn ident(&mut self, what: &str) -> Result<(String, Pos), Diagnostic> {
        let t = self.next(what)?;
        match t.tok {
            Tok::Ident(name) => Ok((name, t.pos)),
            _ => Err(Diagnostic::new(t.pos, what)),
        }
    }

    fn count(&mut self, pos: Pos) -> Result<(), Diagnostic> {
        self.nodes += 1;
        if self.nodes > self.limits.max_commands {
            return Err(Diagnostic::new(
                pos,
                format!(
                    "в скрипте больше {} команд и тестов",
                    self.limits.max_commands
                ),
            ));
        }
        Ok(())
    }

    fn nesting(&self, depth: usize, pos: Pos) -> Result<(), Diagnostic> {
        if depth > self.limits.max_nesting {
            return Err(Diagnostic::new(
                pos,
                format!("вложенность глубже {}", self.limits.max_nesting),
            ));
        }
        Ok(())
    }

    fn commands(&mut self, depth: usize) -> Result<Vec<Command>, Diagnostic> {
        let mut out = Vec::new();
        while let Some(t) = self.toks.peek() {
            if t.tok == Tok::RBrace {
                break;
            }
            out.push(self.command(depth)?);
        }
        Ok(out)
    }

    fn command(&mut self, depth: usize) -> Result<Command, Diagnostic> {
        let (name, pos) = self.ident("ожидается команда")?;
        self.count(pos)?;
        let (args, tests, test_list) = self.arguments(depth)?;
        let t = self.next("ожидается «;» или «{»")?;
        let block = match t.tok {
            Tok::Semi => None,
            Tok::LBrace => {
                self.nesting(depth + 1, t.pos)?;
                let block = self.commands(depth + 1)?;
                let close = self.next("ожидается «}»")?;
                if close.tok != Tok::RBrace {
                    return Err(Diagnostic::new(close.pos, "ожидается «}»"));
                }
                Some(block)
            }
            _ => return Err(Diagnostic::new(t.pos, "ожидается «;» или «{»")),
        };
        Ok(Command {
            name,
            pos,
            args,
            tests,
            test_list,
            block,
        })
    }

    /// `*argument [test / test-list]`.
    fn arguments(&mut self, depth: usize) -> Result<(Vec<Arg>, Vec<Test>, bool), Diagnostic> {
        let mut args = Vec::new();
        loop {
            let arg = self.toks.next_if(|t| {
                matches!(
                    t.tok,
                    Tok::Tag(_) | Tok::Num(_) | Tok::Str(_) | Tok::LBracket
                )
            });
            let Some(t) = arg else { break };
            let pos = t.pos;
            args.push(match t.tok {
                Tok::Tag(name) => Arg::Tag { name, pos },
                Tok::Num(value) => Arg::Number { value, pos },
                Tok::Str(s) => Arg::Strings {
                    values: vec![s],
                    list: false,
                    pos,
                },
                _ => Arg::Strings {
                    values: self.string_list(pos)?,
                    list: true,
                    pos,
                },
            });
        }
        let (single, list) = match self.toks.peek().map(|t| &t.tok) {
            Some(Tok::Ident(_)) => (true, false),
            Some(Tok::LParen) => (false, true),
            _ => (false, false),
        };
        if single {
            let test = self.test(depth + 1)?;
            Ok((args, vec![test], false))
        } else if list {
            let tests = self.test_list(depth + 1)?;
            Ok((args, tests, true))
        } else {
            Ok((args, Vec::new(), false))
        }
    }

    /// Остаток `"[" string *("," string) "]"` после `[`.
    fn string_list(&mut self, open: Pos) -> Result<Vec<String>, Diagnostic> {
        let mut values = Vec::new();
        loop {
            let t = self.next("ожидается строка")?;
            let Tok::Str(s) = t.tok else {
                return Err(Diagnostic::new(t.pos, "ожидается строка"));
            };
            if values.len() >= self.limits.max_list_len {
                return Err(Diagnostic::new(
                    open,
                    format!("в списке больше {} строк", self.limits.max_list_len),
                ));
            }
            values.push(s);
            let t = self.next("ожидается «,» или «]»")?;
            match t.tok {
                Tok::Comma => {}
                Tok::RBracket => return Ok(values),
                _ => return Err(Diagnostic::new(t.pos, "ожидается «,» или «]»")),
            }
        }
    }

    fn test(&mut self, depth: usize) -> Result<Test, Diagnostic> {
        let (name, pos) = self.ident("ожидается тест")?;
        self.nesting(depth, pos)?;
        self.count(pos)?;
        let (args, tests, test_list) = self.arguments(depth)?;
        Ok(Test {
            name,
            pos,
            args,
            tests,
            test_list,
        })
    }

    /// `"(" test *("," test) ")"`.
    fn test_list(&mut self, depth: usize) -> Result<Vec<Test>, Diagnostic> {
        let open = self.next("ожидается «(»")?;
        let mut tests = Vec::new();
        loop {
            if tests.len() >= self.limits.max_list_len {
                return Err(Diagnostic::new(
                    open.pos,
                    format!("в списке больше {} тестов", self.limits.max_list_len),
                ));
            }
            tests.push(self.test(depth)?);
            let t = self.next("ожидается «,» или «)»")?;
            match t.tok {
                Tok::Comma => {}
                Tok::RParen => return Ok(tests),
                _ => return Err(Diagnostic::new(t.pos, "ожидается «,» или «)»")),
            }
        }
    }
}

// ---------- Печать (для снимков и журналов) ----------

impl fmt::Display for Script {
    /// Каноническая запись: команда на строке, отступ блока — два пробела, строки — в нотации Rust.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in &self.commands {
            write_command(f, c, 0)?;
        }
        Ok(())
    }
}

fn write_command(f: &mut fmt::Formatter<'_>, c: &Command, depth: usize) -> fmt::Result {
    write!(f, "{:indent$}{}", "", c.name, indent = depth * 2)?;
    write_args(f, &c.args, &c.tests, c.test_list)?;
    match &c.block {
        None => writeln!(f, ";"),
        Some(block) => {
            writeln!(f, " {{")?;
            for c in block {
                write_command(f, c, depth + 1)?;
            }
            writeln!(f, "{:indent$}}}", "", indent = depth * 2)
        }
    }
}

fn write_args(f: &mut fmt::Formatter<'_>, args: &[Arg], tests: &[Test], list: bool) -> fmt::Result {
    for a in args {
        write!(f, " {a}")?;
    }
    if list {
        write!(f, " (")?;
        for (i, t) in tests.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{t}")?;
        }
        write!(f, ")")?;
    } else {
        for t in tests {
            write!(f, " {t}")?;
        }
    }
    Ok(())
}

impl fmt::Display for Test {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        write_args(f, &self.args, &self.tests, self.test_list)
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arg::Tag { name, .. } => write!(f, ":{name}"),
            Arg::Number { value, .. } => write!(f, "{value}"),
            Arg::Strings {
                values,
                list: false,
                ..
            } if values.len() == 1 => write!(f, "{:?}", values[0]),
            Arg::Strings { values, .. } => {
                write!(f, "[")?;
                for (i, s) in values.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{s:?}")?;
                }
                write!(f, "]")
            }
        }
    }
}
