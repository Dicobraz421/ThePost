# CMP-01: находки проверки клиентом Postman

lane: deploy                # пункт 3 — jmap, пункт 4 — smtp: по картам волн
status: todo
deps: []
class: A
files: [deploy/compose/thepost-smb.yaml, deploy/docker/Dockerfile, deploy/README.md]
read: [deploy/README.md#эталон-smb-в-контейнерах-профиль-smb]
checks: []
manual: no

Проверка 2026-11-05 на `beta-20261105`, отчёт — `docs/THEPOST-COMPAT.md` репозитория Postman. Почта через JMAP работает: вход,
папки, письма, синхронизация.

## Сделать
1. Эталон `--profile smb` не принимает соединений: в `thepost-smb.yaml` нет слушателей (`http.jmap`, `imap`, `smtp`), узел пишет
   «слушатели роли не заданы». Нет `smtp.delivery.local_domains`. Без `/etc/thepost/tls/fullchain.pem` и `privkey.pem` узел не
   стартует, том `tls` их не содержит (там `server.pem`, `server.key`).
2. `/var/lib/thepost/search` не создан: индексатор пишет `Permission denied (os error 13)` каждую секунду, с созданным каталогом
   работает. Похоже на Landlock: несуществующий каталог не попадает в разрешённые [проверить].
3. `EmailSubmission/query` — `unknownMethod` (RFC 8621 §7.3), клиенту он нужен для отложенной отправки и отмены.
4. Отправка локальному адресату через JMAP идёт во внешнюю очередь и MX: на стенде без DNS — `TLSA lookup failed`, письмо
   остаётся `pending`. Локальные домены лучше доставлять сразу.
5. Вопрос: как завести первую учётную запись SMB без OIDC-провайдера? Admin API принимает только Bearer OIDC. При заведении
   напрямую в базе системных папок нет: `Mailbox/get` пуст.
6. Образ: на Debian Landlock не пускает к `/etc/ssl/certs/*` (ссылки в `/usr/share/ca-certificates`); в distroless файл один —
   проверить, не упадёт ли то же на установке под systemd.

## Приёмка
- `docker compose --profile smb up` на чистой машине: JMAP, IMAP и SMTP отвечают, письмо между двумя пользователями доходит.
