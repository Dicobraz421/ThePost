# CMP-01: эталон `--profile smb` принимает соединения

lane: deploy
deps: []
class: A
files: [deploy/compose/thepost-smb.yaml, deploy/compose/tls/*, deploy/docker/Dockerfile, deploy/README.md]
read: [deploy/README.md#эталон-smb-в-контейнерах-профиль-smb]
checks: []
manual: yes

Проверка 2026-11-05 клиентом Postman на `beta-20261105` (отчёт — `docs/THEPOST-COMPAT.md` репозитория Postman). Остальные
находки — CMP-02…06, вопрос о первой учётной записи — issue с меткой `решение`.

## Сделать
1. В `thepost-smb.yaml` — слушатели `http.jmap`, `imap`, `smtp` (25 и submission 587/465) и `smtp.delivery.local_domains`
   (домен эталона). Сейчас узел пишет «слушатели роли не заданы».
2. Файлы TLS: узел ждёт `/etc/thepost/tls/fullchain.pem` и `privkey.pem`, том `tls` содержит `server.pem`, `server.key`.
   Привести к одному: пути в конфиге на файлы тома или выпуск файлов под ожидаемыми именами.
3. Образ: на Debian Landlock не пускает к `/etc/ssl/certs/*` (ссылки в `/usr/share/ca-certificates`); проверить distroless
   и установку под systemd, при необходимости — путь в разрешённые (`confine`) в эталоне, находку в ядре — карточкой sys.
4. `deploy/README.md`: проверка эталона (порты, письмо между двумя пользователями).

## Приёмка
- `docker compose --profile smb up` на чистой машине: JMAP, IMAP и SMTP принимают соединения; ручная проверка —
  `docs/manual-tests/CMP-01.md` (письмо между двумя пользователями доходит, после CMP-04 и CMP-05).
