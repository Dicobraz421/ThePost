# SMTP-05: Milter-клиент — ручная проверка с Rspamd

Linux или WSL2 с Docker. Rspamd принимает Milter на воркере `proxy` (порт 11332, режим `milter = yes` —
включён в образе `rspamd/rspamd` по умолчанию). В `deploy/compose/dev.yml` порт 11332 не опубликован —
запускаем отдельный контейнер.

## Подготовка
1. `docker run -d --name tp-rspamd -p 11332:11332 rspamd/rspamd:3.11.1` → контейнер запущен.
2. config.yaml узла (роль smtp-in и submission, локальный домен `example.org`):
   ```yaml
   smtp:
     hostname: mx.example.org
     inbound:
       - listen: "127.0.0.1:2525"
     delivery:
       local_domains: [example.org]
     milter:
       filters:
         - name: rspamd
           address: "127.0.0.1:11332"
           submission: true
           on_failure: accept
   ```
3. `thepost serve` с этой конфигурацией; учётная запись `bob@example.org` существует.

## Сценарии
1. Обычное письмо: `swaks --server 127.0.0.1:2525 --to bob@example.org --from a@example.net` → `250 2.0.0 OK`;
   в письме у bob есть заголовки Rspamd (`X-Spamd-Result`, `X-Spam-Status` или `X-Rspamd-Queue-Id`).
2. GTUBE: `swaks ... --body 'XJS*C4JDBQADN1.NSBN3*2IDNEN*GTUBE-STANDARD-ANTI-UBE-TEST-EMAIL*C.34X'` →
   отказ `554 5.7.1` (действие `reject` Rspamd, ответ `replycode`), письмо не доставлено.
3. Карантин: в `local.d/actions.conf` Rspamd задать `add_header = 0.1;` и `rewrite_subject = null;`, перезапустить —
   письмо из п. 1 приходит с `X-Spam: Yes`. Для `quarantine` (через модуль `force_actions`) — письмо в «Спам» bob;
   то же письмо через submission — отказ `550 5.7.1 Message held by content filter`.
4. Недоступность, `on_failure: accept`: `docker stop tp-rspamd`, письмо из п. 1 → `250`, в письме
   `X-ThePost-Milter: rspamd; unavailable`, в журнале узла — `milter: фильтр недоступен`.
5. Недоступность, `on_failure: tempfail`: изменить политику (hot reload), повторить п. 4 → `451 4.7.1`.
6. Таймаут: `docker pause tp-rspamd` при `command_timeout: 2s` → через ~2 с ответ по политике п. 4/5;
   `docker unpause tp-rspamd`.

## Уборка
`docker rm -f tp-rspamd`.
