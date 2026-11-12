# CTR-59: занятость повторяющихся событий — правка контракта CTR-47 под разворот при запросе

lane: contracts
deps: [CTR-47]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Контрактная часть META-54 (решение 59), выделена по решению 71 (карточка — одна полоса).

## Сделать
- `FreeBusy::query` и `DavStore` (CTR-47): описание — индекс занятости только у неповторяющихся событий, повторы
  разворачивает реализация на запрошенный интервал; `DavLimits::busy_horizon` — удалить или оставить без действия
  (с записью в CHANGELOG); реализация в памяти — тем же правилом, общий набор тестов.
- Описание `ItipSource::Internal`: и приглашение из почтового клиента с `From` вошедшей учётной записи (решение 78, SMTP-36).
- `docs/CONTRACTS.md#Dav`, CHANGELOG. Потребитель — META-54.

## Приёмка
- CI зелёный; тест набора: еженедельная встреча без конца занята через 3 года; исключённая дата свободна.
