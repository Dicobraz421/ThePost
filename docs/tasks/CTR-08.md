# CTR-08: SASL-интерфейс

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/sasl.rs]
read: [docs/CONTRACTS.md#Auth, docs/spec/S5.md#5.1]
checks: [nextest]
manual: no

## Сделать
- SaslServer (start, step → Challenge|Done(Principal)|Fail), перечень механизмов.

## Приёмка
- Doc-тест обмена PLAIN.
