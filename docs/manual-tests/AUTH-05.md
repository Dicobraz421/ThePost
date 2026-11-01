# AUTH-05: Kerberos GSSAPI — ручная проверка

Нужны: домен FreeIPA (ниже область `CORP.EXAMPLE`, узел почты `mail.corp.example`), Linux-узел почты в домене
(`ipa-client-install`), пакеты MIT Kerberos (`libkrb5-dev` / `krb5-devel` — для сборки, `krb5-user` — для `kinit`).
Подключение механизма в протоколах (`AuthSasl::with_gssapi(Gssapi::system(..))`, `CredentialsRouter::gssapi`) —
волна приложения (APP); до неё проверяются сборка и тесты.

## Сборка
1. `cargo build -p tp-auth --features gssapi` и `cargo nextest run -p tp-auth --features gssapi` — без ошибок.

## Keytab служб
1. `ipa service-add imap/mail.corp.example` и `ipa service-add smtp/mail.corp.example`.
2. `ipa-getkeytab -p imap/mail.corp.example -k /etc/thepost/thepost.keytab`, то же для `smtp/…`;
   `chown thepost: /etc/thepost/thepost.keytab && chmod 600 /etc/thepost/thepost.keytab`.
3. `klist -k /etc/thepost/thepost.keytab` → оба принципала.

## Вход
1. В config.yaml:
   ```yaml
   auth:
     gssapi:
       keytab: /etc/thepost/thepost.keytab
       hostname: mail.corp.example
       realms: { CORP.EXAMPLE: corp.example }
   ```
   и в окружении процесса `KRB5_KTNAME=FILE:/etc/thepost/thepost.keytab` (без неё запуск — ошибка
   `gssapi.keytab: set KRB5_KTNAME=…`).
2. На клиенте: `kinit alice@CORP.EXAMPLE`, затем `imtest -m GSSAPI -a alice@corp.example mail.corp.example`
   (cyrus-imapd-utils) → `Authenticated`; `CAPABILITY` до входа содержит `AUTH=GSSAPI` и без TLS.
3. `swaks --server mail.corp.example:587 --tls --auth GSSAPI --auth-user alice@corp.example` → `235`.
4. Отказы: без билета (`kdestroy`) — клиент не начинает обмен; отключённая учётная запись — IMAP
   `NO [AUTHENTICATIONFAILED]`, SMTP `535 5.7.8`; `imtest -u bob@corp.example` (вход от имени другого) —
   `NO [AUTHORIZATIONFAILED]`.
5. В журнале нет билетов и токенов; при отказе — только `GSSAPI: security context rejected` (уровень debug).
