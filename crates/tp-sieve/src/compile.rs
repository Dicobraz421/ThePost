//! Проверка скрипта и компиляция в программу (SIEVE-02): известные команды и тесты, аргументы, `require` для каждого
//! расширения (RFC 5228 §3.2), компараторы и типы сравнения.

use std::collections::BTreeSet;

use crate::ast::{self, Arg, Command};
use crate::run::VacationAction;
use crate::{Diagnostic, Limits, Pos};

/// Поддерживаемые расширения — их же объявляют ManageSieve (`SIEVE`) и JMAP Sieve (`sieveExtensions`).
pub const EXTENSIONS: &[&str] = &[
    "fileinto",
    "envelope",
    "imap4flags",
    "vacation",
    "body",
    "relational",
    "subaddress",
    "copy",
    "mailbox",
    "encoded-character",
    "comparator-i;ascii-numeric",
];

/// Компараторы ядра: в `require` допустимы, но не обязательны (RFC 5228 §2.7.3).
const IMPLICIT: &[&str] = &["comparator-i;octet", "comparator-i;ascii-casemap"];

/// Проверенная программа: результат [`check`] / [`compile`], вход [`crate::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub(crate) commands: Vec<Cmd>,
    extensions: Vec<String>,
}

impl Program {
    /// Расширения из `require` (без компараторов ядра), по алфавиту.
    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cmd {
    If {
        branches: Vec<(Test, Vec<Cmd>)>,
        otherwise: Option<Vec<Cmd>>,
    },
    Stop,
    Keep {
        flags: Option<Vec<String>>,
    },
    Discard,
    FileInto {
        mailbox: String,
        copy: bool,
        create: bool,
        flags: Option<Vec<String>>,
    },
    Redirect {
        address: String,
        copy: bool,
    },
    Flags {
        op: FlagOp,
        flags: Vec<String>,
    },
    Vacation(VacationAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlagOp {
    Set,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Test {
    True,
    False,
    Not(Box<Test>),
    AllOf(Vec<Test>),
    AnyOf(Vec<Test>),
    Exists(Vec<String>),
    Size {
        over: bool,
        limit: u64,
    },
    Header {
        m: Match,
        names: Vec<String>,
        keys: Vec<String>,
    },
    Address {
        m: Match,
        part: Part,
        names: Vec<String>,
        keys: Vec<String>,
    },
    Envelope {
        m: Match,
        part: Part,
        names: Vec<String>,
        keys: Vec<String>,
    },
    Body {
        m: Match,
        transform: Transform,
        keys: Vec<String>,
    },
    HasFlag {
        m: Match,
        keys: Vec<String>,
    },
    MailboxExists(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Match {
    pub cmp: Comparator,
    pub kind: MatchKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchKind {
    Is,
    Contains,
    Matches,
    Value(Rel),
    Count(Rel),
}

/// Оператор `relational` (RFC 5231).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rel {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comparator {
    Octet,
    AsciiCasemap,
    AsciiNumeric,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    All,
    Local,
    Domain,
    User,
    Detail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transform {
    Raw,
    Text,
    Content(Vec<String>),
}

/// Проверка с лимитами по умолчанию — для ManageSieve `CHECKSCRIPT`, JMAP `SieveScript/validate`, Admin API.
pub fn check(text: &str) -> Result<Program, Vec<Diagnostic>> {
    compile(text.as_bytes(), &Limits::default())
}

/// Разбор и проверка: все найденные ошибки с позициями (ошибка разбора — одна).
pub fn compile(src: &[u8], limits: &Limits) -> Result<Program, Vec<Diagnostic>> {
    let script = ast::parse(src, limits).map_err(|d| vec![d])?;
    let mut c = Compiler::default();
    let commands = c.block(&script.commands, true);
    if c.diags.is_empty() {
        Ok(Program {
            commands,
            extensions: c.exts.into_iter().collect(),
        })
    } else {
        Err(c.diags)
    }
}

// ---------- Аргументы ----------

/// Вид значения тега или позиционного аргумента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Flag,
    Num,
    Str,
    List,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Flag => "ничего",
            Kind::Num => "число",
            Kind::Str => "строка",
            Kind::List => "список строк",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Val<'a> {
    Flag,
    Num(u64),
    Str(&'a str),
    List(&'a [String]),
}

#[derive(Debug, Default)]
struct Parsed<'a> {
    tags: Vec<(&'a str, Pos, Val<'a>)>,
    pos: Vec<(Val<'a>, Pos)>,
}

impl<'a> Parsed<'a> {
    fn get(&self, tag: &str) -> Option<(Val<'a>, Pos)> {
        self.tags
            .iter()
            .find(|(t, _, _)| *t == tag)
            .map(|(_, p, v)| (*v, *p))
    }

    fn has(&self, tag: &str) -> bool {
        self.get(tag).is_some()
    }

    fn num(&self, tag: &str) -> Option<u64> {
        match self.get(tag) {
            Some((Val::Num(n), _)) => Some(n),
            _ => None,
        }
    }
}

fn value(a: &Arg, kind: Kind) -> Option<Val<'_>> {
    match (a, kind) {
        (Arg::Number { value, .. }, Kind::Num) => Some(Val::Num(*value)),
        (
            Arg::Strings {
                values,
                list: false,
                ..
            },
            Kind::Str,
        ) => values.first().map(|s| Val::Str(s.as_str())),
        (Arg::Strings { values, .. }, Kind::List) => Some(Val::List(values.as_slice())),
        _ => None,
    }
}

const MATCH_TAGS: &[(&str, Kind)] = &[
    ("comparator", Kind::Str),
    ("is", Kind::Flag),
    ("contains", Kind::Flag),
    ("matches", Kind::Flag),
    ("value", Kind::Str),
    ("count", Kind::Str),
];

const PART_TAGS: &[(&str, Kind)] = &[
    ("all", Kind::Flag),
    ("localpart", Kind::Flag),
    ("domain", Kind::Flag),
    ("user", Kind::Flag),
    ("detail", Kind::Flag),
];

const BODY_TAGS: &[(&str, Kind)] = &[
    ("raw", Kind::Flag),
    ("content", Kind::List),
    ("text", Kind::Flag),
];

// ---------- Компилятор ----------

#[derive(Debug, Default)]
struct Compiler {
    exts: BTreeSet<String>,
    diags: Vec<Diagnostic>,
    encoded: bool,
}

impl Compiler {
    fn err(&mut self, pos: Pos, message: impl Into<String>) {
        self.diags.push(Diagnostic::new(pos, message));
    }

    fn need(&mut self, ext: &str, pos: Pos) {
        if !self.exts.contains(ext) {
            self.err(pos, format!("расширение {ext:?} не объявлено в require"));
        }
    }

    /// Раскладывает аргументы: теги из `tags` (с значениями), затем позиционные ровно вида `positional`.
    fn args<'a>(
        &mut self,
        what: &str,
        at: Pos,
        args: &'a [Arg],
        tags: &[(&str, Kind)],
        positional: &[Kind],
    ) -> Option<Parsed<'a>> {
        let mut out = Parsed::default();
        let mut ok = true;
        let mut raw = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let Arg::Tag { name, pos } = a else {
                raw.push(a);
                continue;
            };
            let Some(&(_, kind)) = tags.iter().find(|(t, _)| *t == name.as_str()) else {
                self.err(*pos, format!("{what}: неизвестный тег :{name}"));
                ok = false;
                continue;
            };
            if out.has(name) {
                self.err(*pos, format!("{what}: тег :{name} указан дважды"));
                ok = false;
            }
            let val = if kind == Kind::Flag {
                Val::Flag
            } else if let Some(v) = it.next().and_then(|v| value(v, kind)) {
                v
            } else {
                self.err(
                    *pos,
                    format!("{what}: после :{name} ожидается {}", kind.name()),
                );
                ok = false;
                continue;
            };
            out.tags.push((name.as_str(), *pos, val));
        }
        if raw.len() != positional.len() {
            let pos = raw.get(positional.len()).map_or(at, |a| a.pos());
            self.err(
                pos,
                format!(
                    "{what}: ожидается позиционных аргументов: {}, указано: {}",
                    positional.len(),
                    raw.len()
                ),
            );
            return None;
        }
        for (a, kind) in raw.into_iter().zip(positional) {
            match value(a, *kind) {
                Some(v) => out.pos.push((v, a.pos())),
                None => {
                    self.err(a.pos(), format!("{what}: ожидается {}", kind.name()));
                    ok = false;
                }
            }
        }
        ok.then_some(out)
    }

    fn decode(&mut self, s: &str, pos: Pos) -> String {
        if !self.encoded || !s.contains("${") {
            return s.to_owned();
        }
        match decode_encoded(s) {
            Ok(v) => v,
            Err(e) => {
                self.err(pos, e);
                s.to_owned()
            }
        }
    }

    fn string(&mut self, (v, pos): (Val<'_>, Pos)) -> String {
        match v {
            Val::Str(s) => self.decode(s, pos),
            Val::List(l) => l.first().map(|s| self.decode(s, pos)).unwrap_or_default(),
            Val::Flag | Val::Num(_) => String::new(),
        }
    }

    fn list(&mut self, (v, pos): (Val<'_>, Pos)) -> Vec<String> {
        match v {
            Val::List(l) => l.iter().map(|s| self.decode(s, pos)).collect(),
            Val::Str(s) => vec![self.decode(s, pos)],
            Val::Flag | Val::Num(_) => Vec::new(),
        }
    }

    /// Имена заголовков: field-name RFC 5322 (печатные ASCII без двоеточия).
    fn header_names(&mut self, v: (Val<'_>, Pos)) -> Vec<String> {
        let pos = v.1;
        let names = self.list(v);
        for n in &names {
            if n.is_empty() || !n.bytes().all(|b| (33..=126).contains(&b) && b != b':') {
                self.err(pos, format!("недопустимое имя заголовка {n:?}"));
            }
        }
        names
    }

    /// Флаги IMAP: строки списка делятся по пробелам (RFC 5232 §3).
    fn flags(&mut self, v: (Val<'_>, Pos)) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in self.list(v) {
            for f in s.split_ascii_whitespace() {
                if !out.iter().any(|x| x.eq_ignore_ascii_case(f)) {
                    out.push(f.to_owned());
                }
            }
        }
        out
    }

    fn matcher(&mut self, p: &Parsed<'_>) -> Match {
        let mut kind = None;
        for &(tag, pos, val) in &p.tags {
            let k = match tag {
                "is" => MatchKind::Is,
                "contains" => MatchKind::Contains,
                "matches" => MatchKind::Matches,
                "value" | "count" => {
                    self.need("relational", pos);
                    let rel = match val {
                        Val::Str(s) => rel_of(s),
                        _ => None,
                    };
                    let Some(rel) = rel else {
                        self.err(
                            pos,
                            "оператор сравнения — один из \"gt\", \"ge\", \"lt\", \"le\", \"eq\", \"ne\"",
                        );
                        continue;
                    };
                    if tag == "value" {
                        MatchKind::Value(rel)
                    } else {
                        MatchKind::Count(rel)
                    }
                }
                _ => continue,
            };
            if kind.replace(k).is_some() {
                self.err(pos, "указано несколько типов сравнения");
            }
        }
        let kind = kind.unwrap_or(MatchKind::Is);
        let cmp = match p.get("comparator") {
            None => Comparator::AsciiCasemap,
            Some(v) => {
                let pos = v.1;
                let name = self.string(v);
                let cmp = match name.as_str() {
                    "i;octet" => Comparator::Octet,
                    "i;ascii-casemap" => Comparator::AsciiCasemap,
                    "i;ascii-numeric" => {
                        self.need("comparator-i;ascii-numeric", pos);
                        Comparator::AsciiNumeric
                    }
                    other => {
                        self.err(pos, format!("неизвестный компаратор {other:?}"));
                        Comparator::AsciiCasemap
                    }
                };
                if cmp == Comparator::AsciiNumeric
                    && matches!(kind, MatchKind::Contains | MatchKind::Matches)
                {
                    self.err(
                        pos,
                        "компаратор \"i;ascii-numeric\" не поддерживает :contains и :matches",
                    );
                }
                cmp
            }
        };
        Match { cmp, kind }
    }

    fn part(&mut self, p: &Parsed<'_>) -> Part {
        let mut part = None;
        for &(tag, pos, _) in &p.tags {
            let x = match tag {
                "all" => Part::All,
                "localpart" => Part::Local,
                "domain" => Part::Domain,
                "user" | "detail" => {
                    self.need("subaddress", pos);
                    if tag == "user" {
                        Part::User
                    } else {
                        Part::Detail
                    }
                }
                _ => continue,
            };
            if part.replace(x).is_some() {
                self.err(pos, "указано несколько частей адреса");
            }
        }
        part.unwrap_or(Part::All)
    }

    // ---------- Команды ----------

    fn block(&mut self, cmds: &[Command], top: bool) -> Vec<Cmd> {
        let mut out: Vec<Cmd> = Vec::new();
        let mut require_allowed = top;
        let mut open_if = false;
        for c in cmds {
            if c.name == "require" {
                if !require_allowed {
                    self.err(c.pos, "require допустим только в начале скрипта");
                }
                self.require(c);
                continue;
            }
            require_allowed = false;
            match c.name.as_str() {
                "if" => {
                    let test = self.condition(c);
                    let body = self.body(c);
                    out.push(Cmd::If {
                        branches: vec![(test, body)],
                        otherwise: None,
                    });
                    open_if = true;
                }
                "elsif" => {
                    let test = self.condition(c);
                    let body = self.body(c);
                    match out.last_mut() {
                        Some(Cmd::If { branches, .. }) if open_if => branches.push((test, body)),
                        _ => self.err(c.pos, "elsif без предшествующего if"),
                    }
                }
                "else" => {
                    self.bare(c);
                    let body = self.body(c);
                    match out.last_mut() {
                        Some(Cmd::If { otherwise, .. }) if open_if => *otherwise = Some(body),
                        _ => self.err(c.pos, "else без предшествующего if"),
                    }
                    open_if = false;
                }
                _ => {
                    open_if = false;
                    if c.block.is_some() {
                        self.err(c.pos, format!("у команды {} не может быть блока", c.name));
                    }
                    if let Some(t) = c.tests.first() {
                        self.err(t.pos, format!("у команды {} не может быть теста", c.name));
                    }
                    if let Some(cmd) = self.simple(c) {
                        out.push(cmd);
                    }
                }
            }
        }
        out
    }

    fn require(&mut self, c: &Command) {
        if c.block.is_some() || !c.tests.is_empty() {
            self.err(c.pos, "require: ожидается список расширений и «;»");
        }
        let Some(p) = self.args("require", c.pos, &c.args, &[], &[Kind::List]) else {
            return;
        };
        let Some(&(Val::List(list), pos)) = p.pos.first() else {
            return;
        };
        for e in list {
            let e = e.to_ascii_lowercase();
            if EXTENSIONS.contains(&e.as_str()) {
                if e == "encoded-character" {
                    self.encoded = true;
                }
                self.exts.insert(e);
            } else if !IMPLICIT.contains(&e.as_str()) {
                self.err(pos, format!("расширение {e:?} не поддерживается"));
            }
        }
    }

    /// Тест `if`/`elsif`: ровно один, не списком; других аргументов нет.
    fn condition(&mut self, c: &Command) -> Test {
        if let Some(a) = c.args.first() {
            self.err(a.pos(), format!("{}: ожидается тест", c.name));
        }
        match (c.tests.as_slice(), c.test_list) {
            ([t], false) => self.test(t),
            _ => {
                self.err(c.pos, format!("{} ожидает один тест", c.name));
                Test::False
            }
        }
    }

    fn bare(&mut self, c: &Command) {
        if let Some(a) = c.args.first() {
            self.err(a.pos(), format!("у {} нет аргументов", c.name));
        } else if let Some(t) = c.tests.first() {
            self.err(t.pos, format!("у {} нет теста", c.name));
        }
    }

    fn body(&mut self, c: &Command) -> Vec<Cmd> {
        match &c.block {
            Some(b) => self.block(b, false),
            None => {
                self.err(c.pos, format!("{} ожидает блок {{…}}", c.name));
                Vec::new()
            }
        }
    }

    fn simple(&mut self, c: &Command) -> Option<Cmd> {
        let name = c.name.as_str();
        match name {
            "stop" | "discard" => {
                self.args(name, c.pos, &c.args, &[], &[])?;
                Some(if name == "stop" {
                    Cmd::Stop
                } else {
                    Cmd::Discard
                })
            }
            "keep" => {
                let p = self.args(name, c.pos, &c.args, &[("flags", Kind::List)], &[])?;
                let flags = self.flags_tag(&p);
                Some(Cmd::Keep { flags })
            }
            "fileinto" => {
                self.need("fileinto", c.pos);
                let p = self.args(
                    name,
                    c.pos,
                    &c.args,
                    &[
                        ("copy", Kind::Flag),
                        ("create", Kind::Flag),
                        ("flags", Kind::List),
                    ],
                    &[Kind::Str],
                )?;
                let copy = self.copy_tag(&p);
                let create = match p.get("create") {
                    Some((_, pos)) => {
                        self.need("mailbox", pos);
                        true
                    }
                    None => false,
                };
                let flags = self.flags_tag(&p);
                let mailbox = self.string(p.pos[0]);
                if mailbox.is_empty() {
                    self.err(p.pos[0].1, "fileinto: пустое имя папки");
                }
                Some(Cmd::FileInto {
                    mailbox,
                    copy,
                    create,
                    flags,
                })
            }
            "redirect" => {
                let p = self.args(name, c.pos, &c.args, &[("copy", Kind::Flag)], &[Kind::Str])?;
                let copy = self.copy_tag(&p);
                let address = self.string(p.pos[0]);
                if !valid_address(&address) {
                    self.err(
                        p.pos[0].1,
                        format!("redirect: некорректный адрес {address:?}"),
                    );
                }
                Some(Cmd::Redirect { address, copy })
            }
            "setflag" | "addflag" | "removeflag" => {
                self.need("imap4flags", c.pos);
                let p = self.args(name, c.pos, &c.args, &[], &[Kind::List])?;
                let flags = self.flags(p.pos[0]);
                let op = match name {
                    "setflag" => FlagOp::Set,
                    "addflag" => FlagOp::Add,
                    _ => FlagOp::Remove,
                };
                Some(Cmd::Flags { op, flags })
            }
            "vacation" => {
                self.need("vacation", c.pos);
                let p = self.args(
                    name,
                    c.pos,
                    &c.args,
                    &[
                        ("days", Kind::Num),
                        ("subject", Kind::Str),
                        ("from", Kind::Str),
                        ("addresses", Kind::List),
                        ("mime", Kind::Flag),
                        ("handle", Kind::Str),
                    ],
                    &[Kind::Str],
                )?;
                let subject = p.get("subject").map(|v| self.string(v));
                let from = p.get("from").map(|v| self.string(v));
                let handle = p.get("handle").map(|v| self.string(v));
                let addresses = p.get("addresses").map(|v| self.list(v)).unwrap_or_default();
                let reason = self.string(p.pos[0]);
                Some(Cmd::Vacation(VacationAction {
                    days: p.num("days"),
                    subject,
                    from,
                    addresses,
                    mime: p.has("mime"),
                    handle,
                    reason,
                }))
            }
            _ => {
                self.err(c.pos, format!("неизвестная команда {name}"));
                None
            }
        }
    }

    fn copy_tag(&mut self, p: &Parsed<'_>) -> bool {
        match p.get("copy") {
            Some((_, pos)) => {
                self.need("copy", pos);
                true
            }
            None => false,
        }
    }

    fn flags_tag(&mut self, p: &Parsed<'_>) -> Option<Vec<String>> {
        let v = p.get("flags")?;
        self.need("imap4flags", v.1);
        Some(self.flags(v))
    }

    // ---------- Тесты ----------

    fn test(&mut self, t: &ast::Test) -> Test {
        let name = t.name.as_str();
        match name {
            "true" | "false" | "not" | "allof" | "anyof" => {
                if let Some(a) = t.args.first() {
                    self.err(a.pos(), format!("у теста {name} нет аргументов"));
                }
            }
            _ => {
                if let Some(x) = t.tests.first() {
                    self.err(
                        x.pos,
                        format!("у теста {name} не может быть вложенного теста"),
                    );
                }
            }
        }
        match name {
            "true" | "false" => {
                if let Some(x) = t.tests.first() {
                    self.err(
                        x.pos,
                        format!("у теста {name} не может быть вложенного теста"),
                    );
                }
                if name == "true" {
                    Test::True
                } else {
                    Test::False
                }
            }
            "not" => match (t.tests.as_slice(), t.test_list) {
                ([x], false) => Test::Not(Box::new(self.test(x))),
                _ => {
                    self.err(t.pos, "not ожидает один тест");
                    Test::False
                }
            },
            "allof" | "anyof" => {
                if !t.test_list || t.tests.is_empty() {
                    self.err(t.pos, format!("{name} ожидает список тестов в скобках"));
                }
                let tests = t.tests.iter().map(|x| self.test(x)).collect();
                if name == "allof" {
                    Test::AllOf(tests)
                } else {
                    Test::AnyOf(tests)
                }
            }
            "exists" => {
                let Some(p) = self.args(name, t.pos, &t.args, &[], &[Kind::List]) else {
                    return Test::False;
                };
                Test::Exists(self.header_names(p.pos[0]))
            }
            "size" => {
                let Some(p) = self.args(
                    name,
                    t.pos,
                    &t.args,
                    &[("over", Kind::Flag), ("under", Kind::Flag)],
                    &[Kind::Num],
                ) else {
                    return Test::False;
                };
                let over = p.has("over");
                if over == p.has("under") {
                    self.err(t.pos, "size: нужен ровно один из :over и :under");
                }
                let limit = match p.pos[0].0 {
                    Val::Num(n) => n,
                    _ => 0,
                };
                Test::Size { over, limit }
            }
            "header" => {
                let Some(p) =
                    self.args(name, t.pos, &t.args, MATCH_TAGS, &[Kind::List, Kind::List])
                else {
                    return Test::False;
                };
                let m = self.matcher(&p);
                let names = self.header_names(p.pos[0]);
                let keys = self.list(p.pos[1]);
                Test::Header { m, names, keys }
            }
            "address" | "envelope" => {
                if name == "envelope" {
                    self.need("envelope", t.pos);
                }
                let tags: Vec<(&str, Kind)> = MATCH_TAGS.iter().chain(PART_TAGS).copied().collect();
                let Some(p) = self.args(name, t.pos, &t.args, &tags, &[Kind::List, Kind::List])
                else {
                    return Test::False;
                };
                let m = self.matcher(&p);
                let part = self.part(&p);
                let keys = self.list(p.pos[1]);
                if name == "address" {
                    let names = self.header_names(p.pos[0]);
                    Test::Address {
                        m,
                        part,
                        names,
                        keys,
                    }
                } else {
                    let pos = p.pos[0].1;
                    let names: Vec<String> = self
                        .list(p.pos[0])
                        .into_iter()
                        .map(|n| n.to_ascii_lowercase())
                        .collect();
                    for n in &names {
                        if n != "from" && n != "to" {
                            self.err(pos, format!("envelope: неизвестная часть конверта {n:?}"));
                        }
                    }
                    Test::Envelope {
                        m,
                        part,
                        names,
                        keys,
                    }
                }
            }
            "body" => {
                self.need("body", t.pos);
                let tags: Vec<(&str, Kind)> = MATCH_TAGS.iter().chain(BODY_TAGS).copied().collect();
                let Some(p) = self.args(name, t.pos, &t.args, &tags, &[Kind::List]) else {
                    return Test::False;
                };
                let m = self.matcher(&p);
                let mut transform = None;
                for &(tag, pos, val) in &p.tags {
                    let x = match tag {
                        "raw" => Transform::Raw,
                        "text" => Transform::Text,
                        "content" => Transform::Content(self.list((val, pos))),
                        _ => continue,
                    };
                    if transform.replace(x).is_some() {
                        self.err(pos, "body: указано несколько преобразований");
                    }
                }
                let keys = self.list(p.pos[0]);
                Test::Body {
                    m,
                    transform: transform.unwrap_or(Transform::Text),
                    keys,
                }
            }
            "hasflag" => {
                self.need("imap4flags", t.pos);
                let Some(p) = self.args(name, t.pos, &t.args, MATCH_TAGS, &[Kind::List]) else {
                    return Test::False;
                };
                let m = self.matcher(&p);
                let keys = self.flags(p.pos[0]);
                Test::HasFlag { m, keys }
            }
            "mailboxexists" => {
                self.need("mailbox", t.pos);
                let Some(p) = self.args(name, t.pos, &t.args, &[], &[Kind::List]) else {
                    return Test::False;
                };
                Test::MailboxExists(self.list(p.pos[0]))
            }
            _ => {
                self.err(t.pos, format!("неизвестный тест {name}"));
                Test::False
            }
        }
    }
}

fn rel_of(s: &str) -> Option<Rel> {
    Some(match s.to_ascii_lowercase().as_str() {
        "gt" => Rel::Gt,
        "ge" => Rel::Ge,
        "lt" => Rel::Lt,
        "le" => Rel::Le,
        "eq" => Rel::Eq,
        "ne" => Rel::Ne,
        _ => return None,
    })
}

/// Адрес `redirect`: `local@domain` без пробелов и угловых скобок.
fn valid_address(a: &str) -> bool {
    match a.rsplit_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && !a
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ','))
        }
        None => false,
    }
}

/// `${hex:…}` и `${unicode:…}` (RFC 5228 §2.4.2.4). Нераспознанная запись остаётся как есть; код Unicode вне
/// диапазона и не UTF-8 после `hex` — ошибка.
pub(crate) fn decode_encoded(s: &str) -> Result<String, String> {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.extend_from_slice(rest.get(..i).unwrap_or_default().as_bytes());
        let after = rest.get(i + 2..).unwrap_or_default();
        let one = after
            .find('}')
            .and_then(|j| decode_one(after.get(..j).unwrap_or_default()).map(|r| (j, r)));
        match one {
            Some((j, r)) => {
                out.extend(r?);
                rest = after.get(j + 1..).unwrap_or_default();
            }
            None => {
                out.extend_from_slice(b"${");
                rest = after;
            }
        }
    }
    out.extend_from_slice(rest.as_bytes());
    String::from_utf8(out).map_err(|_| "${hex:…} даёт текст не в UTF-8".to_owned())
}

/// Одна запись без `${` и `}`: `None` — не по грамматике.
fn decode_one(inner: &str) -> Option<Result<Vec<u8>, String>> {
    let (name, seq) = inner.split_once(':')?;
    let hex = if name.eq_ignore_ascii_case("hex") {
        true
    } else if name.eq_ignore_ascii_case("unicode") {
        false
    } else {
        return None;
    };
    let items: Vec<&str> = seq.split_ascii_whitespace().collect();
    if items.is_empty()
        || items
            .iter()
            .any(|x| !x.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    if hex {
        if items.iter().any(|x| x.len() > 2) {
            return None;
        }
        return Some(Ok(items
            .iter()
            .filter_map(|x| u8::from_str_radix(x, 16).ok())
            .collect()));
    }
    let mut out = Vec::new();
    for x in items {
        let digits = x.trim_start_matches('0');
        let code = if digits.is_empty() {
            Some(0)
        } else if digits.len() > 6 {
            None
        } else {
            u32::from_str_radix(digits, 16).ok()
        };
        let Some(c) = code.and_then(char::from_u32) else {
            return Some(Err(format!("недопустимый код Unicode {x}")));
        };
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
    Some(Ok(out))
}
