# ADM-28: сессии Admin API через хранилище

lane: admin
deps: [CTR-61]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 77 (этап 3).

## Сделать
- `Local` (вход по токену `admin init`, `POST /api/v1/session`): сессии — в `AdminSessions` вместо `HashMap` процесса;
  без хранилища — реализация в памяти (как сейчас).

## Приёмка
- Тест роутера: сессия, созданная одним экземпляром `Admin`, принимается другим с тем же хранилищем; выход удаляет её.
