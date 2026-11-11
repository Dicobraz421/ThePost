# SIEVE-09: ManageSieve — команды скриптов

lane: sieve
deps: [SIEVE-08]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3, docs/CONTRACTS.md]
checks: [nextest]
manual: docs/manual-tests/SIEVE-09.md

## Сделать
- `PUTSCRIPT` и `CHECKSCRIPT` (проверка SIEVE-02, ответ `NO` с позицией ошибки), `LISTSCRIPTS`, `SETACTIVE`, `GETSCRIPT`,
  `DELETESCRIPT` (активный — `NO (ACTIVE)`), `RENAMESCRIPT`, `HAVESPACE` (лимиты `SieveStore`, `NO (QUOTA/MAXSIZE)`); коды ответов
  RFC 5804 §1.3.
- Ручная проверка (после APP-27): дополнение Sieve для Thunderbird — список, правка, активация; правило `fileinto` срабатывает
  на новом письме.

## Приёмка
- Тесты сеанса на in-memory `SieveStore`: полный цикл, коды `NONEXISTENT`, `ALREADYEXISTS`, `ACTIVE`, `QUOTA`.
