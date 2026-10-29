# META-12: Манифесты индексов

lane: meta
deps: [CTR-09, META-01]
class: B
files: [crates/tp-meta/src/index_manifest.rs, crates/tp-meta/migrations/*]
read: [docs/CONTRACTS.md#Search]
checks: [nextest]
manual: no

## Сделать
- IndexManifestStore с оптимистичной версией (CAS).

## Приёмка
- Тест: конкурентная запись — побеждает одна версия.
