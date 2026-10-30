# SYS-05: serde и tracing в tp-sys

lane: sys
deps: [SYS-04]
class: B
files: [crates/tp-sys/src/*]
read: [docs/decisions/2026-10-30-owner-answers.md#Платформа и наблюдаемость]
checks: [nextest]
manual: no

## Сделать
- Зависимости `serde`, `tracing` — отдельным PR `x/sys-dep`.
- `Deserialize` для `SandboxConfig`, `ListenerConfig` и прочих секций; служебный HTTP платформы получает параметры сокета из конфига.
- Сбои подпроцессов sandbox логировать через `tracing` (вместе с возвратом `JobError`).

## Приёмка
- Тест разбора секций из YAML; журнал сбоя подпроцесса виден в тесте с подписчиком.
