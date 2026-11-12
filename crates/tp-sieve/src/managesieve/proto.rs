//! Синтаксис ManageSieve (RFC 5804 §4): аргументы команды, литералы, строки ответа.

/// Аргумент команды.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Arg {
    /// Слово: имя команды.
    Atom(String),
    /// Строка: в кавычках или литерал.
    Str(Vec<u8>),
    /// Число.
    Num(u64),
}

/// Литерал в конце строки команды.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Literal {
    /// Строка закончена.
    None,
    /// `{n+}`: дальше `n` байт и продолжение строки.
    Size(u64),
    /// Размер не разобран (RFC 5804: число до 4294967295).
    Bad,
}

/// Наибольшая строка в кавычках в ответе (RFC 5804 §4); длиннее — литералом.
const MAX_QUOTED: usize = 1024;

/// Отделяет литерал в конце строки: `PUTSCRIPT "a" {31+}` → (`PUTSCRIPT "a" `, 31). Синхронизирующий `{n}`
/// клиенту запрещён, но принимается так же: продолжения сервер не посылает.
pub(crate) fn split_literal(line: &[u8]) -> (&[u8], Literal) {
    let Some(body) = line.strip_suffix(b"}") else {
        return (line, Literal::None);
    };
    let Some(open) = body.iter().rposition(|&b| b == b'{') else {
        return (line, Literal::None);
    };
    // `{` внутри строки в кавычках литерал не открывает: `NOOP "a{1}` — ошибка синтаксиса, `"x {1+}"` сюда не
    // попадает (строка кончается кавычкой).
    if open > 0 && body[open - 1] != b' ' {
        return (line, Literal::None);
    }
    let digits = &body[open + 1..];
    let digits = digits.strip_suffix(b"+").unwrap_or(digits);
    let size = std::str::from_utf8(digits)
        .ok()
        .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse::<u64>().ok())
        .filter(|&n| n <= u64::from(u32::MAX));
    match size {
        Some(n) => (&line[..open], Literal::Size(n)),
        None => (line, Literal::Bad),
    }
}

/// Разбирает аргументы части строки (без литерала) в `args`; `false` — ошибка синтаксиса.
pub(crate) fn tokenize(line: &[u8], args: &mut Vec<Arg>) -> bool {
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' => {
                i += 1;
                continue;
            }
            b'"' => {
                let mut s = Vec::new();
                i += 1;
                loop {
                    match line.get(i) {
                        Some(b'"') => break,
                        Some(b'\\') if matches!(line.get(i + 1), Some(b'"' | b'\\')) => {
                            s.push(line[i + 1]);
                            i += 2;
                        }
                        Some(&b) if b != b'\\' && b != 0 && b != b'\r' => {
                            s.push(b);
                            i += 1;
                        }
                        _ => return false,
                    }
                }
                i += 1;
                args.push(Arg::Str(s));
            }
            b'0'..=b'9' => {
                let end = token_end(line, i);
                let Some(n) = std::str::from_utf8(&line[i..end])
                    .ok()
                    .and_then(|d| d.parse::<u64>().ok())
                    .filter(|&n| n <= u64::from(u32::MAX))
                else {
                    return false;
                };
                args.push(Arg::Num(n));
                i = end;
            }
            b if b.is_ascii_alphabetic() => {
                let end = token_end(line, i);
                let word = &line[i..end];
                if !word
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                {
                    return false;
                }
                args.push(Arg::Atom(String::from_utf8_lossy(word).into_owned()));
                i = end;
            }
            _ => return false,
        }
        // Между аргументами — пробел.
        if line.get(i).is_some_and(|&b| b != b' ') {
            return false;
        }
    }
    true
}

fn token_end(line: &[u8], from: usize) -> usize {
    line[from..]
        .iter()
        .position(|&b| b == b' ')
        .map_or(line.len(), |p| from + p)
}

/// Пишет строку ответа: в кавычках, а многострочную или длинную — литералом `{n}`.
pub(crate) fn put_string(out: &mut Vec<u8>, s: &str) {
    if s.len() > MAX_QUOTED || s.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        out.extend_from_slice(format!("{{{}}}\r\n", s.len()).as_bytes());
        out.extend_from_slice(s.as_bytes());
        return;
    }
    out.push(b'"');
    for b in s.bytes() {
        if matches!(b, b'"' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Option<Vec<Arg>> {
        let mut out = Vec::new();
        tokenize(line.as_bytes(), &mut out).then_some(out)
    }

    #[test]
    fn arguments() {
        assert_eq!(
            args(r#"HAVESPACE "my \"script\" \\" 1024"#),
            Some(vec![
                Arg::Atom("HAVESPACE".into()),
                Arg::Str(br#"my "script" \"#.to_vec()),
                Arg::Num(1024),
            ])
        );
        assert_eq!(args(""), Some(vec![]));
        assert_eq!(args(r#"NOOP "тег""#).map(|a| a.len()), Some(2));
        for bad in [
            r#"NOOP "open"#,
            r#"NOOP "a"x"#,
            r#"NOOP "a\n""#,
            "NOOP 99999999999",
            "NO*OP",
            "12ab",
            "(x)",
        ] {
            assert_eq!(args(bad), None, "{bad}");
        }
    }

    #[test]
    fn literals() {
        assert_eq!(
            split_literal(b"PUTSCRIPT \"a\" {31+}"),
            (&b"PUTSCRIPT \"a\" "[..], Literal::Size(31))
        );
        assert_eq!(split_literal(b"{5}"), (&b""[..], Literal::Size(5)));
        assert_eq!(split_literal(b"NOOP"), (&b"NOOP"[..], Literal::None));
        assert_eq!(split_literal(b"NOOP \"a{1+}").1, Literal::None);
        assert_eq!(split_literal(b"X {+}").1, Literal::Bad);
        assert_eq!(split_literal(b"X {abc+}").1, Literal::Bad);
        assert_eq!(split_literal(b"X {99999999999+}").1, Literal::Bad);
    }

    #[test]
    fn strings() {
        let enc = |s: &str| {
            let mut out = Vec::new();
            put_string(&mut out, s);
            String::from_utf8(out).expect("utf8")
        };
        assert_eq!(enc(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(enc("a\r\nb"), "{4}\r\na\r\nb");
        assert_eq!(
            enc(&"x".repeat(2000)),
            format!("{{2000}}\r\n{}", "x".repeat(2000))
        );
    }
}
