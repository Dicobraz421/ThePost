# SIEVE-10: тест чтения секции `sieve` из YAML

lane: sieve
deps: [DEPS-08, SIEVE-07]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3]
checks: [nextest]
manual: no

Хвост волны sieve-02, PR #426.

## Сделать
- Тесты `config.rs`: секция `sieve` из YAML (`serde_yaml_ng`, DEPS-08) → `SieveConfig` — пустая секция даёт `Default`;
  заданы лимиты разбора и выполнения, `max_received`, `max_node_hops`, `vacation`, `cache`, `managesieve`
  (`listen`, `max_line`, `max_literal`, `idle_timeout`, `max_connections`, `max_auth_failures`); неизвестное поле и
  неверная длительность — ошибка с именем поля.

## Приёмка
- Тесты проходят; пример секции из документации крейта разбирается без ошибок.
