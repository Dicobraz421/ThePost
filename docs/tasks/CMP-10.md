# CMP-10: приёмка Ф2 — календари, контакты, правила, консоль

lane: deploy
deps: [APP-28, UI-09, DEP-17]
class: A
files: [deploy/*]
read: [docs/spec/APP-G.md, docs/spec/S4.md#4.8, docs/spec/S5.md#5.2]
checks: [nextest]
manual: docs/manual-tests/CMP-10.md

Критерий Ф2: календари, контакты, правила и автоответы проверены клиентами; консоль администратора покрывает Admin API.

## Сделать
- Ручная проверка на бете целиком: Thunderbird, iOS и macOS, DAVx5, Evolution (календари, задачи, контакты, GAL, встречи), Postman
  (JMAP календари, контакты, автоответ), дополнение Sieve для Thunderbird (правила), веб-консоль (все разделы).
- Нагрузка k6 по CalDAV и CardDAV (APP-D) для профиля SMB — отчёт рядом с `criteria.md` DEP-13.
- Найденное — карточки `FIX-<NN>`.

## Приёмка
- Сценарии `docs/manual-tests/CMP-10.md` без «не прошёл»; отчёт нагрузки.
