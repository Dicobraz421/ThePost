# PLT-08: ACME-клиент (SMB) — ручная проверка

Нужны: Linux-ВМ с публичным IPv4, DNS-запись `A` для проверяемого имени (ниже `mail.example.org`) на эту ВМ,
открытый порт 80. Сборка `thepost` с подключённым клиентом ACME (`thepost serve` запускает `Acme::run`, служебный
HTTP включает `acme::challenge_router`) — до волны приложения (APP) проверка откладывается.
Используется **staging** Let's Encrypt: его сертификаты не доверены браузерами, но лимиты выпуска мягкие.

## Выпуск
1. В config.yaml:
   ```yaml
   service_http: { listen: "0.0.0.0:80", internal_networks: ["0.0.0.0/32"] }   # или проброс 80 → 8081; без списка — ошибка (PLT-18)
   tls:
     cert_file: /var/lib/thepost/tls/fullchain.pem
     key_file: /var/lib/thepost/tls/privkey.pem
   acme:
     enabled: true
     directory_url: https://acme-staging-v02.api.letsencrypt.org/directory
     contact: ["mailto:admin@example.org"]
     domains: [mail.example.org]
     state_dir: /var/lib/thepost/acme
   ```
2. Запустить `thepost serve`. В журнале за ≤2 мин — `acme certificate issued`, ошибок `acme request failed` нет.
3. `ls -l /var/lib/thepost/tls /var/lib/thepost/acme` → `fullchain.pem`, `privkey.pem` (права `-rw-------`),
   `account.yaml` (`-rw-------`), `domains` с текстом `mail.example.org`.
4. `openssl x509 -in /var/lib/thepost/tls/fullchain.pem -noout -issuer -enddate -ext subjectAltName` →
   издатель «(STAGING) …», срок ~90 дней (или по профилю CA), SAN `DNS:mail.example.org`.
5. `openssl s_client -connect mail.example.org:993 -servername mail.example.org </dev/null 2>/dev/null | openssl x509 -noout -serial`
   → тот же серийный номер, что у файла (`openssl x509 -in …/fullchain.pem -noout -serial`): TLS перечитан без перезапуска.

## Повторный запуск и продление
1. Перезапустить `thepost serve` → нового выпуска нет (в журнале нет `acme certificate issued`), учётная запись та же
   (`account.yaml` не изменился).
2. Временно задать `acme.renew_before: 365d`, перечитать конфигурацию (`kill -HUP <pid>`) и перезапустить
   (период проверки — 12 ч) → новый выпуск, серийный номер в `openssl s_client` сменился, соединения IMAP не рвались.
3. Добавить второе имя в `acme.domains` (с записью DNS) и перезапустить → перевыпуск с двумя SAN.
4. Вернуть `renew_before: 30d`.

## Ошибки
1. Закрыть порт 80 (или указать имя без DNS-записи) и перезапустить → в журнале `acme request failed` и
   `acme renewal failed`; прежние `fullchain.pem`/`privkey.pem` не изменились, TLS продолжает работать; повтор через
   `retry_interval` (по умолчанию `1h`).
2. `curl -i http://mail.example.org/.well-known/acme-challenge/unknown` → `404`.
