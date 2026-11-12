//! Локальные домены — только принятые домены организации (SMTP-39, решение 96): домен каталога без принятого
//! домена — внешний, ключа `smtp.delivery.local_domains` нет.

use tp_platform::SectionRegistry;

use super::*;

/// Домен каталога (`Example.org` у [`Env`]) без принятого домена организации — внешний: `RCPT` без очереди
/// отклоняется, адрес не разрешается в учётную запись.
#[tokio::test]
async fn directory_domain_without_accepted_is_external() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env
        .delivery(false)
        .with_transport(Arc::new(Routes::new(Arc::new(MemTransport::new()))));
    assert_eq!(
        d.resolve("alice@example.org").await.expect("r"),
        Resolved::External
    );
    let r = d
        .rcpt(&envelope(Mode::Inbound, &[]), "alice@example.org")
        .await;
    assert_ne!(r.code, 250, "{r:?}");
    assert!(env.inbox("alice@example.org").await.is_empty());
}

/// Принятый домен организации — локальная доставка во «Входящие».
#[tokio::test]
async fn accepted_domain_is_local() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(false);
    let e = envelope(Mode::Inbound, &[]);
    assert_eq!(d.rcpt(&e, "alice@example.org").await.code, 250);
    let r = d
        .data(
            &envelope(Mode::Inbound, &["alice@example.org"]),
            Bytes::from_static(MAIL),
        )
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    assert_eq!(env.inbox("alice@example.org").await.len(), 1);
}

/// Ключ `smtp.delivery.local_domains` — ошибка разбора (`thepost config check`); при запуске узла —
/// предупреждение, ключ отбрасывается.
#[test]
fn local_domains_key_is_a_parse_error() {
    let mut r = SectionRegistry::new();
    r.add_section::<SmtpConfig>("smtp").expect("smtp");
    let src = "database: { url: postgres://db/thepost }\nblob: { url: file:///var/lib/thepost }\n\
               domain: example.com\nsmtp:\n  delivery:\n    local_domains: [example.com]\n";
    let err = r
        .parse_strict(src, Vec::new())
        .expect_err("local_domains must fail")
        .to_string();
    assert!(err.contains("smtp.delivery.local_domains"), "{err}");
    let lenient = r.parse(src, Vec::new()).expect("lenient");
    assert!(
        lenient
            .warnings()
            .iter()
            .any(|w| w.contains("smtp.delivery.local_domains")),
        "{:?}",
        lenient.warnings()
    );
}
