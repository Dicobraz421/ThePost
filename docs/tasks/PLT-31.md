# PLT-31: ключ TLS слушателей — только из хранилища секретов

lane: platform
deps: [DEP-44]
class: B
files: [crates/tp-platform/src/*, crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/tasks/PLT-24.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 8; решение 100.

Класс B: удаляется тип, который подставляет узел, — место вызова в `crates/thepost` (`serve.rs`, `doctor*.rs`, `sections.rs`) правится в том же PR
(межполосная по устройству, как контракт с вызовами — решение 71); ветка `x/…`.

## Сделать
- Секция `tls`: `key_file` удалить, только `key: { secret }`; ACME — ключ в хранилище (PLT-29); проверки `doctor` и
  пути Landlock — без файла ключа. Вход узла в OpenBao (`secrets.auth.cert.key_file`) — остаётся файлом.
- `TlsCerts`: один `load(cfg, store)`; `load_with_secrets` удалить, потребителей (`serve.rs`, `doctor.rs`, `doctor_dns.rs`
  и др.) править в этом же PR (хвост реестра, п. 70, PR #547).

## Приёмка
- Тесты: `key_file` в YAML — ошибка разбора; слушатель стартует с ключом из хранилища в памяти.
