# DEPS-18: зависимости метки ответа и BATV — `chacha20poly1305`, `hmac`, `sha1` в tp-smtp

lane: deps
deps: []
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-smtp/Cargo.toml]
read: [docs/decisions/2026-11-11-stage2-followups.md#125, docs/research/2026-11-14-replies-threads.md]
checks: [deny]
manual: no

## Сделать
- `chacha20poly1305` (RustCrypto, XChaCha20-Poly1305: 24-байтный nonce можно брать случайным) — прямой зависимостью
  рабочего пространства на том же `chacha20`, что уже в `Cargo.lock`: второй копии нет.
- `hmac` и `sha1` (BATV — HMAC-SHA1 по draft-levine-smtp-batv-01) — версиями, что уже в дереве.
- `crates/tp-smtp/Cargo.toml` — все три (`workspace = true`). `cargo deny` проходит (лицензии, дубликаты).

## Как делают другие
- Rspamd `replies` — зашифрованная метка в Message-ID (https://github.com/rspamd/rspamd/blob/master/src/plugins/lua/replies.lua);
  BATV — усечённый HMAC-SHA1 (https://datatracker.ietf.org/doc/html/draft-levine-smtp-batv-01). Своей криптографии не
  пишем — готовые крейты RustCrypto (решение 2026-10-31).

## Приёмка
- `cargo check -p tp-smtp` (Linux и Windows) и `cargo deny` зелёные; в `Cargo.lock` по одной версии `chacha20`, `hmac`,
  `sha1`.
