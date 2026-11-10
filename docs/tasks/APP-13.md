# APP-13: подключение расширений IMAP, POP3 и PSI в `serve`

lane: app
deps: [IMAP-12, PLT-17]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: no

Отчёты imap-03 и platform-07.

## Сделать
- IMAP: `Server::with_extensions(Extensions { accounts, acl, authz })` — без этого QUOTA и ACL не объявляются.
- POP3: запуск `Pop3Server` по секции `imap.pop3` (слушатели, STLS) в роли access.
- Нагрузка: `tp_sys::watch_resources` → `pressure::watch_resources`, `rss.run` → `rss.run_on` (PSI по событиям, секция `pressure`).
- Тест сборки сервисов: роль `all` — IMAP объявляет QUOTA и ACL; POP3 отвечает `+OK` при включённой секции.
