# DEPS-02: mail-auth ≥ 0.14 и mail-send 0.7 — без zip 8

lane: deps
deps: [DEPS-01]
class: B
files: [Cargo.toml, Cargo.lock, .github/workflows/ci.yml, crates/tp-smtp/*, crates/thepost/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 2.

## Сделать
- mail-auth → ≥ 0.14, mail-send → 0.7, mail-parser → 1.0 (вместе); поправить вызовы в tp-smtp (SPF, DKIM, DMARC, ARC,
  подпись DKIM) и thepost.
- Убрать пропуски zip < 9 из `cargo audit` (job `deny`).

## Приёмка
- `cargo audit` без пропусков zip; весь `nextest` и сквозной тест `mvp_mail_flow` зелёные.
