# SIEVE-02: проверка скрипта — require, аргументы, сообщения об ошибках

lane: sieve
deps: [SIEVE-01]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3]
checks: [nextest]
manual: no

## Сделать
- Компиляция AST в программу: известные команды и тесты, обязательные и необязательные аргументы, `require` для каждого расширения
  (без `require` — ошибка, RFC 5228 §3.2), допустимые компараторы и типы сравнения.
- Поддерживаемые расширения (их же объявляют ManageSieve и JMAP Sieve): `fileinto`, `envelope`, `imap4flags`, `vacation`, `body`,
  `relational`, `subaddress`, `copy`, `mailbox`, `encoded-character`, `comparator-i;ascii-numeric`.
- `check(text) -> Result<Program, Vec<Diagnostic { line, column, message }>>` — для ManageSieve `CHECKSCRIPT`, JMAP
  `SieveScript/validate`, Admin API.

## Приёмка
- Тесты: каждый вид ошибки с позицией; скрипты, которые генерируют Roundcube и дополнение Sieve для Thunderbird, проходят проверку.
