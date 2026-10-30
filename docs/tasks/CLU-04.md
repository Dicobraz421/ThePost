# CLU-04: шина Valkey только по TLS

lane: cluster
deps: [CLU-01, CTR-12]
class: A
files: [crates/tp-cluster/src/*]
read: [docs/decisions/2026-10-30-owner-answers.md#Шифрование любого обмена]
checks: [nextest]
manual: no

## Сделать
- Фича `redis` с rustls — отдельным PR `x/cluster-dep`.
- Только `rediss://` с проверкой сертификата по CA из секции TLS канала (CTR-12); `redis://` — только dev по `allow_plaintext`, с предупреждением.

## Приёмка
- Тест разбора адреса: `redis://` вне dev отклоняется; интеграционный тест с Valkey (CI-02) — открытый режим dev.
