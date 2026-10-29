# SMTP-02: TLS и submission

lane: smtp
deps: [SMTP-01, AUTH-04, KIT-05]
class: A
files: [crates/tp-smtp/src/server/auth.rs, crates/tp-smtp/src/server/tls.rs]
read: [docs/spec/S4.md#4.1, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

## Сделать
- STARTTLS (25, 587), неявный TLS (465); AUTH только после TLS на 587/465; rate limit попыток.

## Приёмка
- Тесты: AUTH без TLS отклоняется; успешная отправка после AUTH.
