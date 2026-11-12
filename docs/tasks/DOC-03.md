# DOC-03: PREREQUISITES: Kerberos — принципалы служб и keytab

lane: docs
deps: [DEP-39]
class: A
files: [docs/PREREQUISITES.md]
read: [docs/manual-tests/AUTH-05.md]
checks: [none]
manual: no

Хвост реестра облачных волн, п. 39: auth-08, PR #478.

## Сделать
- `docs/PREREQUISITES.md`: принципалы `smtp/`, `imap/`, `HTTP/`, `sieve/<имя узла>` в keytab узла, права файла, проверка
  `klist -k`; ссылка на `docs/manual-tests/AUTH-05.md`.

## Приёмка
- Текст совпадает с шагами ручной проверки AUTH-05.
