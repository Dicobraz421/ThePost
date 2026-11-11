# SIEVE-06: автоответ vacation

lane: sieve
deps: [SIEVE-03]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `vacation` (RFC 5230): `:days` (по умолчанию и пределы — `sieve.vacation`), `:subject`, `:from`, `:addresses`, `:mime`, `:handle`;
  период — через `VacationLog::seen`.
- Не отвечать (RFC 5230 §4.5, RFC 3834): пустой отправитель конверта, `Auto-Submitted` не `no`, заголовки `List-*`,
  `Precedence: bulk|list|junk`, письмо не на адреса пользователя (`:addresses` и адреса учётной записи), свой адрес.
- Ответ `AutoReply`: тема «Автоответ: <тема письма>» по умолчанию, `In-Reply-To`, `References`, `Auto-Submitted: auto-replied`;
  текст — из скрипта или из настройки `Vacation` (CTR-46; её задают JMAP `VacationResponse` и Admin API).

## Приёмка
- Тесты: каждое условие «не отвечать», период `:days`, `:handle`, ответ из настройки `Vacation` с датами `from`/`to`.
