# CTR-52: `InternalTls.key` — ключ внутренних каналов из хранилища секретов

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Internal TLS, docs/CONTRACTS.md#Secrets, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

Хвост волны platform-14, PR #423.

Требует подтверждения владельца: доводить ключ из OpenBao до межузлового канала и клиента к СУБД (вариант А агента), а не только до слушателей? (п. 18 синка 12.11)

## Сделать
- `InternalTls`: поле `key: Option<SecretRef>` (`{ secret: "<путь>#<поле>" }`, PEM) — замена `key_file` (решение 37);
  оба сразу — `Error::Invalid`; `validate`: `cert_file` задаётся вместе с `key_file` или `key`. Ключ открытым текстом
  в `key` — ошибка разбора без значения в тексте (как у секции `tls`, PLT-24).
- Метод, отдающий источник ключа (файл или ссылка на секрет), — чтобы `tp-cluster` и `tp-meta` не разбирали поля сами;
  чтение секрета — у потребителей (CLU-16, META-56), подключение в узле — APP-36.
- Места других крейтов, где `InternalTls` собирается перечислением всех полей, правятся в этом же PR (класс B).
- Схема конфигурации (`JsonSchema`), doc-тест, `docs/CONTRACTS.md#Internal TLS`, CHANGELOG.

## Приёмка
- CI зелёный; тесты `validate`: `key` без `cert_file`, `key` вместе с `key_file`, ключ открытым текстом.
