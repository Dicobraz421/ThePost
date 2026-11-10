# APP-17: подключение журнала доставки, групп, обучения антиспама и защиты от перебора

lane: app
deps: [SMTP-16, SMTP-17, SMTP-18, AUTH-08, APP-16]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: no

Отчёты smtp-05 (SMTP-16…18), auth-04 (AUTH-08), app-15 (APP-16).

## Сделать
- `Delivery::with_groups(Groups::new(..))`, `Delivery::with_tracking(..)`, `QueueWorker::with_tracking(..)`;
  `tp_smtp::learn::SpamLearner` — подписчик событий (пароль controller — из `Registry::secret_store`).
- `AbuseGuard::new(cfg.auth.abuse, AbuseBlocks из PgMeta)`, `guard.listen(bus)`; протоколы (SMTP AUTH, IMAP, POP3, JMAP)
  вызывают `check` до проверки учётных данных, `failure`/`success` после. Хранилище недоступно — по умолчанию пускать
  со счётчиками узла (вопрос владельцу #253).
- `admin.allow_external`: ключ без действия — ошибка разбора с подсказкой (Admin API только на внутреннем адресе, SRS 6.8).
