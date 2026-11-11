# CTR-46: Sieve — скрипты, автоответ, точка правил доставки

lane: contracts
deps: [CTR-05, CTR-10]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/spec/S4.md#4.3, docs/spec/S3.md#3.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

Интерпретатор — свой (SRS 4.3: sieve-rs под AGPL-3.0 не подходит по лицензии), крейт `tp-sieve`; smtp вызывает его через контракт.

## Сделать
- `SieveStore` (по `AccountScope`): `list -> Vec<ScriptInfo { name, size, active }>`, `get(name)`, `put(name, text)` (текст уже проверен
  вызывающим), `rename`, `delete` (активный — `Conflict`), `activate(Option<name>)` (активный не больше одного); лимиты — число скриптов
  и размер (`QuotaExceeded`). Скрипт организации (выполняется до скрипта пользователя): `org_script(tenant)` / `set_org_script`.
- Автоответ (модель JMAP `VacationResponse`, RFC 8621 §8): `Vacation { enabled, from, to, subject, text_body, html_body }`,
  `vacation(scope)` / `set_vacation(scope, Vacation)`; выполняется до активного скрипта.
- `VacationLog::seen(scope, sender, handle, period) -> bool` — атомарно «уже отвечали за период» и отметка (RFC 5230 `:days`).
- Точка правил доставки `DeliveryRules::evaluate(RulesInput) -> RulesOutcome`: вход — получатель (`AccountScope`, адрес с подадресом),
  конверт, доступ к заголовкам и телу (лимит байт), список папок; выход — `keep: Vec<(MailboxRef::{ Id, Name { create } }, Flags)>`,
  `redirect: Vec<String>` (копия или без), `discard`, `vacation: Option<AutoReply { to, from, subject, body, in_reply_to }>`,
  `errors` (ошибка скрипта — implicit keep и запись в журнал). Нет правил — implicit keep во «Входящие».
- Событие `EventKind::SieveChanged` (учётная запись) после изменения скриптов и автоответа — кэш программ tp-sieve, JMAP push.
- ManageSieve — в роли `imap` (как POP3), секция `sieve` крейта tp-sieve; Role не меняется.
- In-memory реализации и общий набор `suite::sieve` в tp-testkit; `FakeRules` для тестов smtp.

## Приёмка
- Набор `suite::sieve` на in-memory: один активный, удаление активного, переименование, лимиты, `VacationLog` за период.
- CHANGELOG в CONTRACTS.md.
