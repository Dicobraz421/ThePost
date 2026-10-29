# PLT-02: Hot reload конфигурации

lane: platform
deps: [PLT-01, SYS-01]
class: A
files: [crates/tp-platform/src/reload.rs]
read: [docs/spec/S5.md#5.5]
checks: [nextest]
manual: no

## Сделать
- По SIGHUP и опросу mtime перечитать файл, проверить, разослать через watch; ошибка — сохранить прежнюю версию.

## Приёмка
- Тест: изменение порога доходит до подписчика; битый файл не ломает текущую конфигурацию.
