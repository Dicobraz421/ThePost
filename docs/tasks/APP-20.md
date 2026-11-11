# APP-20: fail_closed защиты от перебора в узле, устаревший TODO doctor

lane: app
deps: [AUTH-11, APP-08]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 5; отчёты auth-06 и platform-11.

## Сделать
- `services.rs`, `Guarded::verify`: ошибка `self.guard.check(..)` возвращается вызывающему (`Err(e) => return Err(e)`),
  а не только пишется в журнал; doc-комментарий `Guarded` — решение по `fail_closed` принимает `AbuseGuard`.
- `doctor.rs`: снять устаревший `TODO(SYS)` про `/sys/kernel/security/lsm` (проба Landlock — PLT-21).

## Приёмка
- Тест: `auth.abuse.fail_closed: true` и недоступное хранилище блокировок — вход запрещён; `false` — вход проверяется.
