# DOC-13: ADMIN-GUIDE — раздел «Ответы на наши письма и беседы»

lane: docs
deps: [APP-77, DEP-56]
class: A
files: [docs/ADMIN-GUIDE.md]
read: [docs/decisions/2026-11-11-stage2-followups.md#125, docs/research/2026-11-14-replies-threads.md, docs/tasks/SMTP-51.md, docs/tasks/ADM-50.md]
checks: []
manual: no

## Сделать
- Раздел «Ответы на наши письма и беседы» в `docs/ADMIN-GUIDE.md` по коду: когда письмо считается ответом (три
  условия), что это даёт (балл Rspamd, значение «только ответ» в матрице доверия) и чего не даёт (антивирус, URL,
  DMARC — всегда); аномалии, снимающие признак.
- BATV: что это, переход `log` → `enforce`, исключения для рассылок, как понять отказ `550 5.7.1` в журнале доставки.
- Ключи метки и BATV: где лежат, как сменить, что будет без ключа.
- Беседы: удержание и снятие, выгрузка, действия по беседе для пользователя; команды `thepost admin thread …` и
  маршруты; строка «только ответ на наше письмо» — в разделе «Кто с кем и что может».
- «Как делают другие» и атаки на доверие к ответу — ссылками на исследование `docs/research/2026-11-14-replies-threads.md`.

## Как делают другие
- Раздел — по образцу справок Rspamd `replies` и Microsoft о списках разрешённых: что даёт признак и где он
  бессилен (https://docs.rspamd.com/modules/replies,
  https://learn.microsoft.com/en-us/defender-office-365/create-safe-sender-lists-in-office-365).

## Приёмка
- Команды, маршруты, ключи конфигурации и символы Rspamd раздела совпадают с кодом и `deploy/` (`thepost admin thread
  --help`, `openapi_spec()`, секции `smtp.reply`, `smtp.batv`).
