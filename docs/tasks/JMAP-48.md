# JMAP-48: Thread — неизменяемый `threadId` и действия по беседе

lane: jmap
deps: [CTR-94, MS-21]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#125, docs/tasks/CTR-94.md, docs/tasks/MS-21.md]
checks: [nextest]
manual: no

## Сделать
- `Thread/get` и `Thread/changes` (RFC 8621 §3), `Email.threadId` — по привязке беседы (`ThreadStore`), а не по
  хэшу `thread_key`; `threadId` письма не меняется никогда, `Thread/changes` — по журналу изменений беседы.
- Действия по беседе — возможность `urn:thepost:thread`: объект `ThreadRule` (`threadId`, `action`: `alwaysMove` с
  `mailboxId`, `ignore`, `alwaysKeyword` с `keyword`), методы `ThreadRule/get|set|changes` — правило действует на
  будущие письма беседы (MS-21) и по запросу (`applyToExisting`) на прежние, одной операцией `Email/set`.
- Удержание беседы пользователю не видно и не снимается им: письма удержанной беседы удаляются в корзину как обычно,
  окончательное удаление запрещает хук (CTR-94).

## Как делают другие
- JMAP Thread (https://www.rfc-editor.org/rfc/rfc8621.html), атомарность `/set` — по объектам
  (https://www.rfc-editor.org/rfc/rfc8620.html); EWS `ApplyConversationAction` — AlwaysMove, AlwaysDelete, категории
  (https://learn.microsoft.com/en-us/exchange/client-developer/web-service-reference/applyconversationaction-operation);
  Gmail threads (https://developers.google.com/gmail/api/guides/threads).
- Отличие: «игнорировать» переносит будущие письма беседы в корзину, а не удаляет навсегда — окончательное удаление
  только по сроку корзины.

## Приёмка
- Тесты: ответ получает `threadId` исходного; `threadId` не меняется после нового письма, ссылающегося на две беседы;
  `ThreadRule` `alwaysMove` переносит следующее письмо беседы; без `urn:thepost:thread` методы неизвестны
  (`unknownMethod`).
