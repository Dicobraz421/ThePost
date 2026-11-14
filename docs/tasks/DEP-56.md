# DEP-56: Rspamd — баллы за признак ответа и аномалии; ключи меток и BATV в поставке

lane: deploy
deps: [SMTP-51]
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#125, docs/tasks/SMTP-51.md, deploy/ansible/roles/rspamd/tasks/main.yml]
checks: [lint]
manual: no

## Сделать
- Роль `rspamd`: правило `multimap` по заголовку узла — `THEPOST_REPLY` (−4, как `REPLY` модуля `replies`) и
  `THEPOST_REPLY_ANOMALY` (+3); веса — переменными роли. Модуль Rspamd `replies` не включаем: признак ставит узел,
  второй таблицы Message-ID в Redis нет.
- Ключи метки Message-ID и BATV — создание в OpenBao при установке (как ключи DKIM), ротация — задачей роли
  `thepost`; секции `smtp.reply` и `smtp.batv` в шаблоне `config.yaml` (BATV по умолчанию `log`, затем `enforce`).

## Как делают другие
- Rspamd `replies`, символ `REPLY` −4 (https://docs.rspamd.com/modules/replies); `known_senders` −1
  (https://docs.rspamd.com/modules/known_senders).

## Приёмка
- `ansible-lint` и проверка шаблонов; на стенде: письмо с `X-ThePost-Reply` от узла получает `THEPOST_REPLY`,
  тот же заголовок, пришедший извне, — нет (его удаляет узел до Milter).
