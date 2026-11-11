//! Выполнение программы над письмом (SIEVE-03): `if`/`elsif`/`else`, `stop`, действия и implicit keep
//! (RFC 5228 §2.10.2), тесты ядра, компараторы `i;octet`, `i;ascii-casemap`, `i;ascii-numeric` и типы сравнения
//! `:is`, `:contains`, `:matches`, `:value`, `:count`.
//!
//! Заголовки декодируются (RFC 2047), адреса разбираются по RFC 5322 — `mail-parser`. Ошибка выполнения (в том числе
//! превышение [`Limits::max_steps`] и [`Limits::max_actions`]) отменяет действия скрипта: остаётся implicit keep.
//! Слияние действий в `RulesOutcome` — SIEVE-04.

use std::cmp::Ordering;

use mail_parser::{Address, HeaderName, HeaderValue, MessageParser};
use serde::{Deserialize, Serialize};

use crate::Limits;
use crate::compile::{Cmd, Comparator, FlagOp, Match, MatchKind, Part, Program, Rel, Test};

/// Письмо и конверт на входе выполнения.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context<'a> {
    /// Письмо целиком (RFC 5322).
    pub message: &'a [u8],
    /// Отправитель конверта (`MAIL FROM`), пустой — уведомление о недоставке.
    pub envelope_from: &'a str,
    /// Получатель конверта с подадресом (`anna+news@example.org`).
    pub envelope_to: &'a str,
    /// Существующие папки (`mailboxexists`), путь через `/`.
    pub mailboxes: &'a [String],
}

/// Параметры `vacation` из скрипта (RFC 5230); решение «отвечать ли» — SIEVE-06.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VacationAction {
    /// `:days`.
    pub days: Option<u64>,
    /// `:subject`.
    pub subject: Option<String>,
    /// `:from`.
    pub from: Option<String>,
    /// `:addresses`.
    pub addresses: Vec<String>,
    /// `:mime` — `reason` содержит MIME-сущность.
    pub mime: bool,
    /// `:handle`.
    pub handle: Option<String>,
    /// Текст ответа.
    pub reason: String,
}

/// Действие скрипта в порядке выполнения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// `keep` во «Входящие» с флагами.
    Keep {
        /// Флаги IMAP.
        flags: Vec<String>,
    },
    /// `discard`.
    Discard,
    /// `fileinto`.
    FileInto {
        /// Папка.
        mailbox: String,
        /// `:copy` — implicit keep не отменяется.
        copy: bool,
        /// `:create`.
        create: bool,
        /// Флаги IMAP.
        flags: Vec<String>,
    },
    /// `redirect`.
    Redirect {
        /// Адрес.
        address: String,
        /// `:copy` — implicit keep не отменяется.
        copy: bool,
    },
    /// `vacation`.
    Vacation(VacationAction),
}

/// Итог выполнения.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Outcome {
    /// Действия в порядке выполнения; при ошибке — пусто.
    pub actions: Vec<Action>,
    /// Implicit keep: ни `keep`, ни `discard`, ни `fileinto`/`redirect` без `:copy` не выполнены, или ошибка.
    pub implicit_keep: bool,
    /// Флаги implicit keep — внутренняя переменная `imap4flags` в конце выполнения.
    pub flags: Vec<String>,
    /// Ошибка выполнения (без содержимого письма).
    pub error: Option<String>,
}

/// Выполняет программу над письмом.
pub fn run(program: &Program, ctx: &Context<'_>, limits: &Limits) -> Outcome {
    let mut ex = Exec {
        ctx: *ctx,
        limits,
        headers: parse_headers(ctx.message),
        steps: 0,
        actions: Vec::new(),
        flags: Vec::new(),
        cancel_keep: false,
        vacation: false,
    };
    match ex.block(&program.commands) {
        Ok(_) => Outcome {
            implicit_keep: !ex.cancel_keep,
            actions: ex.actions,
            flags: ex.flags,
            error: None,
        },
        Err(Fail(e)) => Outcome {
            implicit_keep: true,
            error: Some(e),
            ..Outcome::default()
        },
    }
}

struct Hdr {
    name: String,
    /// Значение после RFC 2047, без ведущих и хвостовых пробелов.
    value: String,
    /// Границы сырого значения в письме.
    start: usize,
    end: usize,
}

struct Fail(String);

enum Flow {
    Next,
    Stop,
}

struct Exec<'a> {
    ctx: Context<'a>,
    limits: &'a Limits,
    headers: Vec<Hdr>,
    steps: u64,
    actions: Vec<Action>,
    flags: Vec<String>,
    cancel_keep: bool,
    vacation: bool,
}

impl Exec<'_> {
    fn step(&mut self, n: u64) -> Result<(), Fail> {
        self.steps = self.steps.saturating_add(n);
        if self.steps > self.limits.max_steps {
            return Err(steps_exceeded(self.limits));
        }
        Ok(())
    }

    fn block(&mut self, cmds: &[Cmd]) -> Result<Flow, Fail> {
        for c in cmds {
            self.step(1)?;
            if let Flow::Stop = self.cmd(c)? {
                return Ok(Flow::Stop);
            }
        }
        Ok(Flow::Next)
    }

    fn cmd(&mut self, c: &Cmd) -> Result<Flow, Fail> {
        match c {
            Cmd::If {
                branches,
                otherwise,
            } => {
                for (test, body) in branches {
                    if self.test(test)? {
                        return self.block(body);
                    }
                }
                if let Some(body) = otherwise {
                    return self.block(body);
                }
            }
            Cmd::Stop => return Ok(Flow::Stop),
            Cmd::Keep { flags } => {
                let flags = flags.clone().unwrap_or_else(|| self.flags.clone());
                self.act(Action::Keep { flags }, true)?;
            }
            Cmd::Discard => self.act(Action::Discard, true)?,
            Cmd::FileInto {
                mailbox,
                copy,
                create,
                flags,
            } => {
                let flags = flags.clone().unwrap_or_else(|| self.flags.clone());
                self.act(
                    Action::FileInto {
                        mailbox: mailbox.clone(),
                        copy: *copy,
                        create: *create,
                        flags,
                    },
                    !*copy,
                )?;
            }
            Cmd::Redirect { address, copy } => self.act(
                Action::Redirect {
                    address: address.clone(),
                    copy: *copy,
                },
                !*copy,
            )?,
            Cmd::Flags { op, flags } => self.set_flags(*op, flags),
            Cmd::Vacation(v) => {
                if self.vacation {
                    return Err(Fail("vacation выполнен второй раз".into()));
                }
                self.vacation = true;
                self.act(Action::Vacation(v.clone()), false)?;
            }
        }
        Ok(Flow::Next)
    }

    fn act(&mut self, action: Action, cancels_keep: bool) -> Result<(), Fail> {
        if self.actions.len() >= self.limits.max_actions {
            return Err(Fail(format!(
                "больше {} действий за выполнение",
                self.limits.max_actions
            )));
        }
        self.actions.push(action);
        self.cancel_keep |= cancels_keep;
        Ok(())
    }

    fn set_flags(&mut self, op: FlagOp, flags: &[String]) {
        match op {
            FlagOp::Set => {
                self.flags.clear();
                add_flags(&mut self.flags, flags);
            }
            FlagOp::Add => add_flags(&mut self.flags, flags),
            FlagOp::Remove => self
                .flags
                .retain(|f| !flags.iter().any(|x| x.eq_ignore_ascii_case(f))),
        }
    }

    fn test(&mut self, t: &Test) -> Result<bool, Fail> {
        self.step(1)?;
        Ok(match t {
            Test::True => true,
            Test::False => false,
            Test::Not(x) => !self.test(x)?,
            Test::AllOf(tests) => {
                for x in tests {
                    if !self.test(x)? {
                        return Ok(false);
                    }
                }
                true
            }
            Test::AnyOf(tests) => {
                for x in tests {
                    if self.test(x)? {
                        return Ok(true);
                    }
                }
                false
            }
            Test::Exists(names) => {
                self.step(len_u64(self.headers.len()))?;
                names
                    .iter()
                    .all(|n| self.headers.iter().any(|h| h.name.eq_ignore_ascii_case(n)))
            }
            Test::Size { over, limit } => {
                let size = len_u64(self.ctx.message.len());
                if *over { size > *limit } else { size < *limit }
            }
            Test::Header { m, names, keys } => {
                let values = self.header_values(names)?;
                self.matches(m, &values, keys)?
            }
            Test::Address {
                m,
                part,
                names,
                keys,
            } => {
                let values = self.address_values(names, *part)?;
                self.matches(m, &values, keys)?
            }
            Test::Envelope {
                m,
                part,
                names,
                keys,
            } => {
                let values: Vec<String> = names
                    .iter()
                    .filter_map(|n| {
                        let raw = if n == "from" {
                            self.ctx.envelope_from
                        } else {
                            self.ctx.envelope_to
                        };
                        address_part(strip_angle(raw), *part)
                    })
                    .collect();
                self.matches(m, &values, keys)?
            }
            Test::Body { .. } => {
                return Err(Fail("тест body в этой версии не выполняется".into()));
            }
            Test::HasFlag { m, keys } => {
                let values = self.flags.clone();
                self.matches(m, &values, keys)?
            }
            Test::MailboxExists(names) => names.iter().all(|n| {
                self.ctx.mailboxes.iter().any(|mb| {
                    mb == n || (mb.eq_ignore_ascii_case("INBOX") && n.eq_ignore_ascii_case("INBOX"))
                })
            }),
        })
    }

    fn header_values(&mut self, names: &[String]) -> Result<Vec<String>, Fail> {
        self.step(len_u64(self.headers.len()))?;
        Ok(self
            .headers
            .iter()
            .filter(|h| names.iter().any(|n| h.name.eq_ignore_ascii_case(n)))
            .map(|h| h.value.clone())
            .collect())
    }

    fn address_values(&mut self, names: &[String], part: Part) -> Result<Vec<String>, Fail> {
        self.step(len_u64(self.headers.len()))?;
        let mut out = Vec::new();
        for h in &self.headers {
            if !names.iter().any(|n| h.name.eq_ignore_ascii_case(n)) {
                continue;
            }
            let raw = self.ctx.message.get(h.start..h.end).unwrap_or_default();
            out.extend(
                parse_addresses(raw)
                    .iter()
                    .filter_map(|a| address_part(a, part)),
            );
        }
        Ok(out)
    }

    /// Сравнение значений с ключами: истина, если подошла хоть одна пара (для `:count` — число значений).
    fn matches(&mut self, m: &Match, values: &[String], keys: &[String]) -> Result<bool, Fail> {
        if let MatchKind::Count(rel) = m.kind {
            let n = values.len().to_string();
            return Ok(keys.iter().any(|k| rel.holds(compare(m.cmp, &n, k))));
        }
        for k in keys {
            let pattern = matches!(m.kind, MatchKind::Matches).then(|| glob_pattern(k));
            for v in values {
                self.step(1 + len_u64(v.len()) / 256)?;
                let hit = match m.kind {
                    MatchKind::Is => compare(m.cmp, v, k) == Ordering::Equal,
                    MatchKind::Contains => contains(m.cmp, v, k),
                    MatchKind::Value(rel) => rel.holds(compare(m.cmp, v, k)),
                    MatchKind::Matches => {
                        let chars: Vec<char> = v.chars().collect();
                        let p = pattern.as_deref().unwrap_or_default();
                        match glob(p, &chars, m.cmp, &mut self.steps, self.limits.max_steps) {
                            Some(hit) => hit,
                            None => return Err(steps_exceeded(self.limits)),
                        }
                    }
                    MatchKind::Count(_) => false,
                };
                if hit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

fn steps_exceeded(limits: &Limits) -> Fail {
    Fail(format!(
        "превышен лимит шагов выполнения ({})",
        limits.max_steps
    ))
}

fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn add_flags(to: &mut Vec<String>, flags: &[String]) {
    for f in flags {
        if !to.iter().any(|x| x.eq_ignore_ascii_case(f)) {
            to.push(f.clone());
        }
    }
}

impl Rel {
    fn holds(self, o: Ordering) -> bool {
        match self {
            Rel::Gt => o == Ordering::Greater,
            Rel::Ge => o != Ordering::Less,
            Rel::Lt => o == Ordering::Less,
            Rel::Le => o != Ordering::Greater,
            Rel::Eq => o == Ordering::Equal,
            Rel::Ne => o != Ordering::Equal,
        }
    }
}

/// Порядок по компаратору (RFC 4790 §9): `i;ascii-numeric` — по числу из ведущих цифр, строка без цифр в начале —
/// «бесконечность».
fn compare(cmp: Comparator, a: &str, b: &str) -> Ordering {
    match cmp {
        Comparator::Octet => a.as_bytes().cmp(b.as_bytes()),
        Comparator::AsciiCasemap => a
            .bytes()
            .map(|c| c.to_ascii_lowercase())
            .cmp(b.bytes().map(|c| c.to_ascii_lowercase())),
        Comparator::AsciiNumeric => match (numeric(a), numeric(b)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
        },
    }
}

/// Ведущие цифры без ведущих нулей; `None` — строка не начинается с цифры.
fn numeric(s: &str) -> Option<&str> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some(s.get(..end).unwrap_or_default().trim_start_matches('0'))
}

fn contains(cmp: Comparator, v: &str, k: &str) -> bool {
    match cmp {
        Comparator::Octet => v.contains(k),
        Comparator::AsciiCasemap => v.to_ascii_lowercase().contains(&k.to_ascii_lowercase()),
        Comparator::AsciiNumeric => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Glob {
    Star,
    One,
    Lit(char),
}

/// Шаблон `:matches`: `*` — любая последовательность, `?` — один символ, `\` экранирует следующий символ.
fn glob_pattern(k: &str) -> Vec<Glob> {
    let mut out = Vec::new();
    let mut it = k.chars();
    while let Some(c) = it.next() {
        let g = match c {
            '*' => Glob::Star,
            '?' => Glob::One,
            '\\' => Glob::Lit(it.next().unwrap_or('\\')),
            c => Glob::Lit(c),
        };
        if g == Glob::Star && out.last() == Some(&Glob::Star) {
            continue;
        }
        out.push(g);
    }
    out
}

/// Сопоставление с шаблоном за O(n·m) с возвратом к последней `*`; `None` — исчерпан лимит шагов.
fn glob(p: &[Glob], s: &[char], cmp: Comparator, steps: &mut u64, max: u64) -> Option<bool> {
    let eq = |a: char, b: char| match cmp {
        Comparator::Octet => a == b,
        Comparator::AsciiCasemap | Comparator::AsciiNumeric => a.eq_ignore_ascii_case(&b),
    };
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while let Some(&c) = s.get(si) {
        *steps = steps.saturating_add(1);
        if *steps > max {
            return None;
        }
        match p.get(pi) {
            Some(Glob::Star) => {
                star = Some((pi + 1, si));
                pi += 1;
                continue;
            }
            Some(Glob::One) => {
                pi += 1;
                si += 1;
                continue;
            }
            Some(Glob::Lit(x)) if eq(*x, c) => {
                pi += 1;
                si += 1;
                continue;
            }
            _ => {}
        }
        let Some((sp, ss)) = star else {
            return Some(false);
        };
        let ss = ss + 1;
        star = Some((sp, ss));
        pi = sp;
        si = ss;
    }
    Some(
        p.get(pi..)
            .unwrap_or_default()
            .iter()
            .all(|g| *g == Glob::Star),
    )
}

// ---------- Письмо ----------

/// Заголовки верхнего уровня: имя, значение после RFC 2047 и границы сырого значения.
fn parse_headers(raw: &[u8]) -> Vec<Hdr> {
    // Непустая карта заголовков включает разбор «по умолчанию» для всех остальных — как текст с RFC 2047.
    let parser = MessageParser::new()
        .header_text(HeaderName::Subject)
        .default_header_text();
    let Some(msg) = parser.parse_headers(raw) else {
        return Vec::new();
    };
    msg.headers()
        .iter()
        .map(|h| Hdr {
            name: h.name().to_owned(),
            value: header_text(h.value()),
            start: h.offset_start() as usize,
            end: h.offset_end() as usize,
        })
        .collect()
}

fn header_text(v: &HeaderValue<'_>) -> String {
    match v {
        HeaderValue::Text(s) => s.trim().to_owned(),
        HeaderValue::TextList(l) => l.join(", "),
        _ => String::new(),
    }
}

/// Адреса из сырого значения заголовка (RFC 5322, группы раскрываются); имена и комментарии отбрасываются.
fn parse_addresses(raw: &[u8]) -> Vec<String> {
    let mut buf = Vec::with_capacity(raw.len() + 9);
    buf.extend_from_slice(b"From:");
    buf.extend_from_slice(raw);
    buf.extend_from_slice(b"\r\n\r\n");
    let parser = MessageParser::new().header_address(HeaderName::From);
    let Some(msg) = parser.parse_headers(&buf) else {
        return Vec::new();
    };
    let addrs: Vec<&mail_parser::Addr<'_>> = match msg.from() {
        Some(Address::List(list)) => list.iter().collect(),
        Some(Address::Group(groups)) => groups.iter().flat_map(|g| g.addresses.iter()).collect(),
        None => Vec::new(),
    };
    addrs
        .into_iter()
        .filter_map(|a| a.address.as_deref())
        .map(str::to_owned)
        .collect()
}

fn strip_angle(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('<')
        .and_then(|x| x.strip_suffix('>'))
        .unwrap_or(s)
}

/// Часть адреса (RFC 5228 §2.7.4, RFC 5233): локальная часть — до последней `@`, `:user`/`:detail` — по первому `+`;
/// `:detail` без `+` — нет значения.
fn address_part(addr: &str, part: Part) -> Option<String> {
    let (local, domain) = addr.rsplit_once('@').unwrap_or((addr, ""));
    let v = match part {
        Part::All => addr,
        Part::LocalPart => local,
        Part::Domain => domain,
        Part::User => local.split_once('+').map_or(local, |(u, _)| u),
        Part::Detail => local.split_once('+')?.1,
    };
    Some(v.to_owned())
}
