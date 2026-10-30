# CTR-11: tokio, tokio-util и serde_json в контрактах

lane: contracts
deps: [CTR-10]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-10-30-owner-answers.md#Контракты и данные, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Зависимости `tokio` (sync), `tokio-util`, `serde_json` — отдельным PR `x/contracts-dep` (Cargo.toml крейта + Cargo.lock).
- `RoleCtx`: `watch::Receiver` вместо `ConfigWatch` (Any), `CancellationToken` вместо `Shutdown` на futures.
- `ModuleCtx::config::<T: DeserializeOwned>()`; `serde_json::Value` в JMAP-capability и телах Admin API модулей.
- Смена типов ломает вызовы в tp-platform и tp-testkit, поэтому волна идёт веткой `x/contracts-04` (пересечение полос): контракт и правки вызовов — одним PR.

## Приёмка
- Тесты контрактов зелёные; serde round-trip без `schemars::_private::serde_json`.
