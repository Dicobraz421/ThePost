# SMTP-02: Роль submission и TLS

lane: smtp
deps: [SMTP-01, AUTH-04, KIT-05]
class: A
files: [crates/tp-smtp/src/server/auth.rs, crates/tp-smtp/src/server/tls.rs]
read: [docs/spec/S4.md#4.1, docs/spec/S2.md#2.1, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

## Сделать
- STARTTLS (25), отдельная роль submission: 587 (STARTTLS) и 465 (неявный TLS), AUTH только после TLS, лимит частоты отправки и попыток.

## Приёмка
- Тесты: AUTH без TLS отклоняется; успешная отправка после AUTH.
