# SIEVE-08: ManageSieve — протокол и вход

lane: sieve
deps: [SIEVE-02]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Сервер RFC 5804 (`RoleService`, слушатель открывает приложение): приветствие с `IMPLEMENTATION`, `SIEVE` (расширения SIEVE-02),
  `SASL`, `STARTTLS`, `VERSION "1.0"`; команды `CAPABILITY`, `STARTTLS`, `AUTHENTICATE` (`PLAIN`, `OAUTHBEARER` — через контракт SASL,
  как в IMAP), `UNAUTHENTICATE`, `LOGOUT`, `NOOP`.
- Строки и литералы `{n+}`, лимиты строки и литерала, таймаут бездействия — секция `sieve.managesieve`; вход без TLS запрещён
  (кроме `profile: dev`).

## Приёмка
- Тесты сеанса через `tokio::io::duplex`: приветствие, STARTTLS, вход, неверный пароль, длинный литерал.
