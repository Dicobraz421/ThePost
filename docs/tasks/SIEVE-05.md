# SIEVE-05: тесты envelope, body, relational, subaddress

lane: sieve
deps: [SIEVE-03]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3]
checks: [nextest]
manual: no

## Сделать
- `envelope` (RFC 5228 §5.4): `from`, `to` по конверту доставки (`RulesInput`).
- `body` (RFC 5173): `:raw`, `:content`, `:text`; текст частей — с декодированием, объём — не больше `sieve.max_body_scan`.
- `relational` (RFC 5231): `:count`, `:value` с `i;ascii-numeric`.
- `subaddress` (RFC 5233): `:user`, `:detail` (разделитель `+`, как в доставке SMTP).

## Приёмка
- Таблица тестов на корпусе testkit: многочастные письма, кодировки, конверт с подадресом.
