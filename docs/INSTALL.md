# Установка The Post

Для администратора: как поставить сервер, завести первого администратора и пользователей, подключить почтовые
клиенты. Подробности по каждому компоненту — [`deploy/README.md`](../deploy/README.md).

## Что понадобится

- **Linux** x86_64 или AArch64. Для рабочего сервера — Debian 12 или Ubuntu 24.04, от 4 vCPU и 8 ГБ памяти
  (целевой размер для 1 000 пользователей), диск — под письма.
- **Имя сервера** (например, `mail.example.ru`) и **почтовый домен** (`example.ru`), доступ к их DNS.
- **Открытые порты**: 25 (приём почты), 465 и 587 (отправка), 993 и 143 (IMAP), 443 (JMAP, автонастройка),
  80 (только для выпуска сертификата Let's Encrypt).

Все внутренние компоненты ставятся вместе с сервером: PostgreSQL (метаданные), OpenBao (секреты), Rspamd (антиспам).

## Попробовать: Docker Compose

Самый быстрый способ посмотреть — на любой машине с Docker Engine и Compose v2, от 4 ГБ памяти. Домен в этом
варианте тестовый — `example.test`, сертификаты выпускает свой тестовый центр.

```bash
git clone https://github.com/Dicobraz421/ThePost && cd ThePost
docker compose -f deploy/compose/dev.yml --profile smb up -d --build
```

Первая сборка образа идёт 20–30 минут (Rust собирается внутри Docker, на машине его ставить не нужно). Когда все
контейнеры запущены, проверьте окружение:

```bash
docker compose -f deploy/compose/dev.yml --profile smb run --rm --no-deps thepost doctor
```

В итоге не должно быть строк `FAIL`. Предупреждения `WARN` про DKIM и DNS для тестового домена — ожидаемы.

Дальше — [первый администратор](#первый-администратор). В этом варианте команды узла запускаются через
`docker compose -f deploy/compose/dev.yml --profile smb exec thepost thepost …`, а CA для клиентов —
`deploy/compose/tls/ca.pem`.

Остановить — `… --profile smb down`; данные остаются в томах Docker и вернутся при следующем `up`.

## Рабочий сервер: Ansible

Чистая ВМ Debian 12 или Ubuntu 24.04 → PostgreSQL 16, OpenBao, Rspamd (по желанию — антивирус ClamAV) и `thepost`
под systemd, одной командой с управляющей машины. Повторный запуск ничего не ломает — им же применяются изменения.

1. Скачайте выпуск и распакуйте бинарный файл:

   ```bash
   git clone https://github.com/Dicobraz421/ThePost && cd ThePost
   V=beta-20261111.2   # последний выпуск — на странице Releases
   curl -LO https://github.com/Dicobraz421/ThePost/releases/download/$V/thepost-$V-x86_64-unknown-linux-gnu.tar.gz
   tar xzf thepost-$V-x86_64-unknown-linux-gnu.tar.gz
   ```

2. Опишите сервер:

   ```bash
   cp deploy/ansible/inventory.example.yml inventory.yml
   ```

   В `inventory.yml` — адрес ВМ и пользователь SSH, `thepost_domain` (почтовый домен), `thepost_hostname`
   (имя сервера), `thepost_tls_mode` и `thepost_acme_email`:

   | `thepost_tls_mode` | Сертификат |
   |---|---|
   | `certbot` (по умолчанию) | Let's Encrypt; порт 80 открыт, DNS имени сервера указывает на ВМ |
   | `acme` | Let's Encrypt встроенным клиентом The Post |
   | `files` | свои файлы PEM: `thepost_tls_cert_src`, `thepost_tls_key_src` |
   | `internal` | внутренний центр сертификации — только для стенда |

3. Запустите:

   ```bash
   ansible-galaxy collection install -r deploy/ansible/requirements.yml
   ansible-playbook -i inventory.yml deploy/ansible/smb.yml \
     -e thepost_binary_src=$PWD/thepost-$V-x86_64-unknown-linux-gnu/thepost
   ```

   Плейбук сам дождётся готовности узла. Ключи восстановления OpenBao он кладёт в `/root/openbao-init.json` на ВМ —
   **перенесите их в надёжное место и удалите с сервера.**

4. Проверьте на ВМ: `sudo -u thepost thepost doctor` — без `FAIL`.

## Вручную под systemd

Если PostgreSQL и OpenBao уже есть или нужна своя схема — установка бинарного файла, юнита systemd и `config.yaml`
по шагам описана в [`deploy/README.md`](../deploy/README.md#установка-под-systemd-smb-enterprise-вм). Секреты (пароли
СУБД, ключи) в `config.yaml` не пишутся — только ссылки на OpenBao.

## Первый администратор

На сервере, от root или от учётной записи службы:

```bash
sudo thepost admin init --email admin@example.ru
```

Команда заводит администратора и принятый домен и один раз выводит:

- **пароль** администратора — им можно войти в почту по IMAP, SMTP и JMAP;
- **одноразовый токен** Admin API (`tpa_…`), действует 10 минут.

Сохраните оба значения сразу: повторно они не показываются. Повторный запуск откажет (`409`); заново —
`thepost admin init --force`, при этом прежний пароль `admin init` перестанет действовать.

## Учётные записи

Admin API слушает только локальный адрес сервера (`127.0.0.1:8443`), запросы — с самого сервера. Обменяйте
токен на сессию (8 часов):

```bash
A=https://127.0.0.1:8443/api/v1
ORG=00000000-0000-0000-0000-000000000001        # организация по умолчанию
curl -sk -X POST -H "Authorization: Bearer tpa_…" $A/session   # в ответе — "session": "tps_…"
S=tps_…
```

Создайте пользователя и пароль для его почтового клиента:

```bash
curl -sk -X POST -H "Authorization: Bearer $S" -H 'Content-Type: application/json' \
  -d '{"login":"ivanov@example.ru"}' $A/orgs/$ORG/accounts            # в ответе — "id"
curl -sk -X POST -H "Authorization: Bearer $S" -H 'Content-Type: application/json' \
  -d '{"label":"Thunderbird"}' $A/orgs/$ORG/accounts/<id>/app-passwords   # в ответе — "password", один раз
```

У каждого клиента может быть свой пароль приложения; отозвать его можно, не трогая остальные.

В Docker Compose те же запросы выполняются из контейнера в сети узла:

```bash
DC="docker compose -f deploy/compose/dev.yml --profile smb"
ADM="docker run --rm --network container:$($DC ps -q thepost) curlimages/curl:8.11.1 -sk"
$ADM -X POST -H "Authorization: Bearer tpa_…" https://127.0.0.1:8443/api/v1/session
```

> Скоро вместо `curl` — команда `thepost admin` (учётные записи, домены, пароли).

## Настройка клиентов

| | Сервер | Порт | Защита |
|---|---|---|---|
| Входящая почта (IMAP) | `mail.example.ru` | 993 | SSL/TLS |
| | | 143 | STARTTLS |
| Исходящая почта (SMTP) | `mail.example.ru` | 465 | SSL/TLS |
| | | 587 | STARTTLS |
| JMAP | `https://mail.example.ru/.well-known/jmap` | 443 | HTTPS |

Имя входа — адрес целиком (`ivanov@example.ru`), пароль — пароль приложения. Подходит любой клиент IMAP и SMTP
(например, Thunderbird), по JMAP — [Postman](https://github.com/Dicobraz421/Postman). В тестовом варианте с `example.test` клиент попросит принять
сертификат: его выпустил тестовый центр (`deploy/compose/tls/ca.pem`).

## DNS домена

Чтобы почта приходила и не попадала в спам у получателей:

| Запись | Значение |
|---|---|
| `MX` домена | `10 mail.example.ru.` |
| `A` / `AAAA` сервера | адрес ВМ; обратная запись (PTR) адреса — `mail.example.ru` |
| `TXT` домена (SPF) | `v=spf1 mx -all` |
| `TXT _dmarc` | `v=DMARC1; p=quarantine; rua=mailto:postmaster@example.ru` |
| `TXT <селектор>._domainkey` (DKIM) | открытый ключ подписи, см. ниже |

**Подпись DKIM.** Закрытый ключ хранится в OpenBao, в `config.yaml` — только ссылка на него:

```yaml
smtp:
  dkim:
    domains:
      example.ru:
        selector: s2026
        keys:
          s2026: { algorithm: rsa-sha256, key: { secret: "thepost/dkim#example.ru" } }
```

`thepost doctor` проверяет MX, SPF, DKIM, DMARC, MTA-STS и PTR по публичному DNS и подсказывает, чего не хватает.

## Обновление

- **Ansible** — скачайте новый выпуск и повторите `ansible-playbook … -e thepost_binary_src=…`: узел перезапустится,
  миграции базы применятся сами.
- **Docker Compose** — `git pull` и `… --profile smb up -d --build`.

Перед обновлением рабочего сервера сделайте резервную копию PostgreSQL и каталога писем.

## Проверка подписи

Архивы, SBOM и образ подписаны [cosign](https://docs.sigstore.dev/) без ключей; рядом с каждым файлом лежит
`<файл>.sigstore.json`. Готовая команда проверки — в описании каждого выпуска, например:

```bash
f=thepost-$V-x86_64-unknown-linux-gnu.tar.gz
cosign verify-blob "$f" --bundle "$f.sigstore.json" \
  --certificate-identity 'https://github.com/Dicobraz421/ThePost/.github/workflows/beta.yml@refs/heads/main' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

## Если что-то не так

- `thepost doctor` — первая проверка: что именно не так и как исправить.
- Журнал узла — `journalctl -u thepost` (в Docker — `docker compose … logs thepost`); пароли и токены в него не пишутся.
- Вопросы и ошибки — в [issues](https://github.com/Dicobraz421/ThePost/issues).
