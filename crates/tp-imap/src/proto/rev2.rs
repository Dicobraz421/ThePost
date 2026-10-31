//! Команды IMAP4rev2, которых нет в грамматике imap-codec (IMAP4rev1 и расширения):
//! расширенный `LIST` (RFC 5258: опции выбора, несколько шаблонов, `RETURN`; RFC 5819 `STATUS`;
//! RFC 6154 `SPECIAL-USE`), `CREATE … (USE (…))` (RFC 6154) и `STATUS` с `SIZE` (RFC 8438).
//!
//! Разбирается сообщение, которое imap-codec не принял, целиком (строки и литералы уже собраны
//! нарезкой потока); прочие команды — `None`, сессия отвечает `BAD`.

use super::types::IntoStatic;
use super::types::core::Tag;
use super::{Body, Command, ListCommand, StatusItem};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// Атом или `list-mailbox` (допускает `%`, `*`, `]`, `\`).
    Atom(Vec<u8>),
    /// Строка в кавычках или литерал.
    Str(Vec<u8>),
    Open,
    Close,
}

/// Лексер сообщения: токены через пробел, в конце CRLF.
fn tokens(mut s: &[u8]) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    loop {
        match s.first()? {
            b'\r' => return (s == b"\r\n").then_some(out),
            b' ' => s = &s[1..],
            b'(' => {
                out.push(Tok::Open);
                s = &s[1..];
            }
            b')' => {
                out.push(Tok::Close);
                s = &s[1..];
            }
            b'"' => {
                let mut v = Vec::new();
                let mut i = 1;
                loop {
                    match *s.get(i)? {
                        b'"' => break,
                        b'\\' => {
                            let c = *s.get(i + 1)?;
                            if c != b'"' && c != b'\\' {
                                return None;
                            }
                            v.push(c);
                            i += 2;
                        }
                        b'\r' | b'\n' | 0 => return None,
                        c => {
                            v.push(c);
                            i += 1;
                        }
                    }
                }
                out.push(Tok::Str(v));
                s = &s[i + 1..];
            }
            b'{' => {
                let close = s.iter().position(|&c| c == b'}')?;
                let spec = &s[1..close];
                let digits = spec
                    .strip_suffix(b"+")
                    .or(spec.strip_suffix(b"-"))
                    .unwrap_or(spec);
                if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                    return None;
                }
                let len: usize = std::str::from_utf8(digits).ok()?.parse().ok()?;
                let rest = s.get(close + 1..)?.strip_prefix(b"\r\n")?;
                out.push(Tok::Str(rest.get(..len)?.to_vec()));
                s = &rest[len..];
            }
            _ => {
                let end = s
                    .iter()
                    .position(|&c| {
                        matches!(c, b' ' | b'(' | b')' | b'"' | b'{' | b'\r' | b'\n' | 0)
                    })
                    .unwrap_or(s.len());
                if end == 0 {
                    return None;
                }
                out.push(Tok::Atom(s[..end].to_vec()));
                s = &s[end..];
            }
        }
    }
}

struct P {
    toks: std::vec::IntoIter<Tok>,
    peeked: Option<Tok>,
}

impl P {
    fn next(&mut self) -> Option<Tok> {
        self.peeked.take().or_else(|| self.toks.next())
    }

    fn peek(&mut self) -> Option<&Tok> {
        if self.peeked.is_none() {
            self.peeked = self.toks.next();
        }
        self.peeked.as_ref()
    }

    fn atom(&mut self) -> Option<String> {
        match self.next()? {
            Tok::Atom(a) => String::from_utf8(a).ok().map(|a| a.to_ascii_uppercase()),
            _ => None,
        }
    }

    /// `astring` / `list-mailbox`.
    fn string(&mut self) -> Option<Vec<u8>> {
        match self.next()? {
            Tok::Atom(a) | Tok::Str(a) => Some(a),
            _ => None,
        }
    }

    fn open(&mut self) -> Option<()> {
        (self.next()? == Tok::Open).then_some(())
    }

    fn eat_open(&mut self) -> bool {
        if self.peek() == Some(&Tok::Open) {
            self.peeked = None;
            true
        } else {
            false
        }
    }

    /// Атомы до `)`; пустой список допускается.
    fn atoms(&mut self) -> Option<Vec<String>> {
        let mut v = Vec::new();
        loop {
            if self.peek() == Some(&Tok::Close) {
                self.peeked = None;
                return Some(v);
            }
            v.push(self.atom()?);
        }
    }

    fn done(&mut self) -> bool {
        self.next().is_none()
    }
}

fn status_items(p: &mut P) -> Option<Vec<StatusItem>> {
    p.open()?;
    let items = p
        .atoms()?
        .iter()
        .map(|a| StatusItem::parse(a))
        .collect::<Option<Vec<_>>>()?;
    (!items.is_empty()).then_some(items)
}

/// Разбор команды IMAP4rev2, которую не принял imap-codec.
pub(super) fn parse(msg: &[u8]) -> Option<Command> {
    let mut p = P {
        toks: tokens(msg)?.into_iter(),
        peeked: None,
    };
    let Tok::Atom(tag) = p.next()? else {
        return None;
    };
    let tag = Tag::try_from(tag.as_slice()).ok()?.into_static();
    let body = match p.atom()?.as_str() {
        "LIST" => Body::List(list(&mut p)?),
        "CREATE" => {
            let mailbox = p.string()?;
            let mut uses = Vec::new();
            if p.eat_open() {
                // create-param = "USE" SP "(" use-attr *(SP use-attr) ")"
                if p.atom()? != "USE" {
                    return None;
                }
                p.open()?;
                uses = p.atoms()?;
                if p.next()? != Tok::Close {
                    return None;
                }
            }
            Body::Create { mailbox, uses }
        }
        "STATUS" => {
            let mailbox = p.string()?;
            Body::Status {
                mailbox,
                items: status_items(&mut p)?,
            }
        }
        _ => return None,
    };
    p.done().then_some(Command { tag, body })
}

/// `LIST [SP "(" opts ")"] SP mailbox SP (pattern | "(" patterns ")") [SP RETURN SP "(" opts ")"]`.
fn list(p: &mut P) -> Option<ListCommand> {
    let mut cmd = ListCommand {
        extended: true,
        ..ListCommand::default()
    };
    if p.eat_open() {
        for opt in p.atoms()? {
            match opt.as_str() {
                "SUBSCRIBED" => cmd.select_subscribed = true,
                "REMOTE" => {}
                "RECURSIVEMATCH" => cmd.recursive = true,
                "SPECIAL-USE" => cmd.select_special_use = true,
                _ => return None,
            }
        }
        // RFC 5258, 3.1: RECURSIVEMATCH только вместе с другой опцией выбора.
        if cmd.recursive && !cmd.select_subscribed && !cmd.select_special_use {
            return None;
        }
    }
    cmd.reference = p.string()?;
    if p.eat_open() {
        loop {
            if p.peek() == Some(&Tok::Close) {
                p.next();
                break;
            }
            cmd.patterns.push(p.string()?);
        }
        if cmd.patterns.is_empty() {
            return None;
        }
    } else {
        cmd.patterns.push(p.string()?);
    }
    if p.peek().is_some() {
        if p.atom()? != "RETURN" {
            return None;
        }
        p.open()?;
        loop {
            match p.next()? {
                Tok::Close => break,
                Tok::Atom(a) => match a.to_ascii_uppercase().as_slice() {
                    b"SUBSCRIBED" => cmd.return_subscribed = true,
                    b"CHILDREN" => cmd.return_children = true,
                    b"SPECIAL-USE" => cmd.return_special_use = true,
                    b"STATUS" => cmd.return_status = Some(status_items(p)?),
                    _ => return None,
                },
                _ => return None,
            }
        }
    }
    Some(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Option<Command> {
        parse(s.as_bytes())
    }

    #[test]
    fn list_extended_rfc5258() {
        // RFC 5258, 5, пример 4.
        let c = cmd("A04 LIST (SUBSCRIBED RECURSIVEMATCH) \"\" \"%\" RETURN (CHILDREN)\r\n")
            .expect("parsed");
        assert_eq!(c.tag.inner(), "A04");
        let Body::List(l) = c.body else {
            panic!("not list")
        };
        assert!(l.extended && l.select_subscribed && l.recursive && l.return_children);
        assert_eq!(l.reference, b"");
        assert_eq!(l.patterns, vec![b"%".to_vec()]);
        // Несколько шаблонов (пример 8) и литерал.
        let c = cmd("BBB LIST \"\" (\"INBOX\" {11}\r\nDesktop/%/* ) RETURN (SUBSCRIBED)\r\n")
            .expect("parsed");
        let Body::List(l) = c.body else {
            panic!("not list")
        };
        assert_eq!(l.patterns, vec![b"INBOX".to_vec(), b"Desktop/%/*".to_vec()]);
        assert!(l.return_subscribed);
    }

    #[test]
    fn list_status_and_special_use() {
        // RFC 5819, пример; RFC 6154, 5.1.
        let c = cmd("A01 LIST \"\" % RETURN (STATUS (MESSAGES UNSEEN SIZE))\r\n").expect("parsed");
        let Body::List(l) = c.body else {
            panic!("not list")
        };
        assert_eq!(
            l.return_status,
            Some(vec![
                StatusItem::Messages,
                StatusItem::Unseen,
                StatusItem::Size
            ])
        );
        let c = cmd("t1 LIST (SPECIAL-USE) \"\" \"*\"\r\n").expect("parsed");
        let Body::List(l) = c.body else {
            panic!("not list")
        };
        assert!(l.select_special_use);
        // RECURSIVEMATCH без другой опции выбора — ошибка (RFC 5258, 3.1).
        assert!(cmd("t2 LIST (RECURSIVEMATCH) \"\" \"*\"\r\n").is_none());
    }

    #[test]
    fn create_use_and_status_size() {
        // RFC 6154, 5.2.
        let c = cmd("t1 CREATE MySpecial (USE (\\Drafts \\Sent))\r\n").expect("parsed");
        assert_eq!(
            c.body,
            Body::Create {
                mailbox: b"MySpecial".to_vec(),
                uses: vec!["\\DRAFTS".into(), "\\SENT".into()],
            }
        );
        // RFC 8438, 3.
        let c = cmd("a STATUS \"INBOX\" (MESSAGES SIZE)\r\n").expect("parsed");
        assert_eq!(
            c.body,
            Body::Status {
                mailbox: b"INBOX".to_vec(),
                items: vec![StatusItem::Messages, StatusItem::Size],
            }
        );
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "a NOOP\r\n",
            "a LIST\r\n",
            "a LIST \"\" (\r\n",
            "a STATUS INBOX ()\r\n",
            "a STATUS INBOX (FOO)\r\n",
            "a CREATE x (USE (\\Sent)\r\n",
            "a LIST \"\" * RETURN (FOO)\r\n",
            "a LIST \"\" {5}\r\nab\r\n",
            "a LIST \"\" *",
        ] {
            assert!(cmd(s).is_none(), "{s:?}");
        }
    }
}
