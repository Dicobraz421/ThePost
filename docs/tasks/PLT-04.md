# PLT-04: Служебный HTTP: метрики и health

lane: platform
deps: [PLT-03, CTR-04]
class: A
files: [crates/tp-platform/src/service_http.rs]
read: [docs/spec/S6.md#6.1, docs/spec/S6.md#6.4]
checks: [nextest]
manual: no

## Сделать
- axum-сервер /metrics, /healthz/live, /healthz/ready; ReadyHandle ролей; ready=false при drain.

## Приёмка
- Тест: ready меняется при регистрации и drain роли.
