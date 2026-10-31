# CTR-17: ссылка на секрет в файле для узлов edge

lane: contracts
deps: [CTR-15]
class: B
files: [crates/tp-contracts/src/secrets.rs, docs/CONTRACTS.md, crates/tp-testkit/src/secrets.rs, crates/tp-platform/src/secrets.rs, crates/tp-platform/src/config.rs, crates/thepost/src/secrets.rs]
read: [docs/decisions/2026-10-31-secrets-availability-edge.md, docs/spec/S2.md#2.6, docs/spec/S6.md#6.7, docs/CONTRACTS.md#Secrets]
checks: [nextest]
manual: no

## Сделать
- `SecretRef` — второй вариант источника: в конфигурации `{ file: "<абсолютный путь>" }` (`deny_unknown_fields`), значение —
  всё содержимое файла (без завершающего перевода строки). `{ secret: ... }` и `{ file: ... }` вместе — ошибка разбора;
  скаляр на месте ссылки — по-прежнему «secret in plaintext». В БД — только вариант хранилища: строка `file:` в `FromStr` — ошибка.
- Допустимость: вариант `file` разрешён только при наборе ролей, равном пресету `edge`; на прочих узлах — ошибка
  `thepost config check` и запуска с указанием поля (проверка — в tp-platform при сборке конфигурации).
- Чтение файла: только при запуске и перечитывании конфигурации; права файла шире `0600` или владелец не процесс — ошибка;
  содержимое — `Secret` (обнуление, `***` в `Debug`). Путь из учётных данных systemd (`$CREDENTIALS_DIRECTORY`) допустим.
- `thepost doctor` на узле edge: файлы существуют, права `0600`, сертификат канала не истекает в ближайшие 30 сут (порог — в секции).
- `docs/CONTRACTS.md#Secrets` и журнал изменений контрактов; вызовы в tp-testkit, tp-platform, thepost — в том же PR
  (ветка `x/contracts-07`).

## Приёмка
- Тесты разбора: `secret`, `file`, оба поля, скаляр, `file:` в `FromStr`; отказ `file` на пресете `mailbox`; права файла.
- Доставка секретов edge от узлов transport (SRS 2.6) — вне карточки, полоса smtp, Ф3.
