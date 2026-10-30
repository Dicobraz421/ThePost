//! Общее для тестов приложения: адрес тестовой БД для профиля dev, перенос пароля в хранилище секретов.

/// Адрес тестовой БД без `sslmode` — открытый канал (`sslmode=disable`, профиль dev).
pub fn dev_url(url: &str) -> String {
    if url.contains("sslmode=") {
        return url.to_owned();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}sslmode=disable")
}

/// Адрес без пароля и пароль (с раскрытыми `%XX`), если он был в `user:password@`.
pub fn split_password(url: &str) -> (String, Option<String>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_owned(), None);
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return (url.to_owned(), None);
    };
    let Some((user, password)) = userinfo.split_once(':') else {
        return (url.to_owned(), None);
    };
    (
        format!("{scheme}://{user}@{host}{tail}"),
        Some(percent_decode(password)),
    )
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn password_split() {
    assert_eq!(
        split_password("postgres://u:p%40w@h:5432/db"),
        ("postgres://u@h:5432/db".to_owned(), Some("p@w".to_owned()))
    );
    assert_eq!(split_password("postgres://u@h/db").1, None);
    assert_eq!(
        dev_url("postgres://h/db"),
        "postgres://h/db?sslmode=disable"
    );
}
