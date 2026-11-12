# APP-33: правила Sieve и автоответ в Admin API узла

lane: app
deps: [ADM-14, ADM-17, META-49]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Сборщик Admin API узла (`services.rs`, `tp_admin::Admin::new`) подключает `Admin::with_sieve`: хранилище —
  `SieveStore` из `tp-meta`, проверка текста — `tp_sieve::check` (`Diagnostic` → `tp_admin::ScriptError`).
  Сейчас вызова нет: маршруты `/sieve/...`, `/vacation` и команды `thepost admin sieve` (ADM-17) на узле
  отвечают 503 (решение 62).
- Проверка текста — та же функция, что у ManageSieve и JMAP: один разбор, без копии в `thepost`.

## Приёмка
- Тест: Admin API, собранный сборщиком узла, на `GET .../sieve/scripts` отвечает 200 (не 503); запись
  неверного скрипта — 422 с позицией ошибки от `tp_sieve::check`.
