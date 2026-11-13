# SMTP-45: submission — только свои адреса отправителя (аналог `reject_sender_login_mismatch`)

lane: smtp
deps: [SMTP-36]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/spec/APP-ZH.md, docs/releases/0.2.0.md]
checks: [nextest]
manual: no

Приёмка беты 0.2.0, CMP-01, ошибка 1: после `AUTH` submission (587, 465) принимал `MAIL FROM` и `From` коллеги и
внешнего домена, письмо доставлялось. JMAP в том же случае отвечает `forbiddenFrom` и `forbiddenMailFrom`.

## Сделать
- `MAIL FROM` сеанса submission после входа — свой адрес, псевдоним (`Account::addresses`) или адрес с правом
  Send As; пустой (`<>`) — можно. Иначе — `553 5.7.1` на `MAIL`.
- Заголовок `From` — свой адрес, псевдоним, Send As или Send on Behalf; `Sender` — как `MAIL FROM`. Иначе —
  `550 5.7.1` на данных (RFC 5321, 4.3.2: `553` на `DATA` не предусмотрен). Send on Behalf без `Sender` — узел
  добавляет `Sender` с именем входа, как JMAP.
- Права — `Authorizer::send_right` через `Delivery::with_authz`, та же проверка, что у приглашений (SMTP-36) и JMAP;
  второй копии нет.
- smtp-in (25, без входа) и приложение без SASL (адреса — `sources`) не затрагиваются.
- Права через группы в `Delivery::submit` — CTR-82, здесь не обходятся.

## Приёмка
- Тесты `deliver::tests::sender_login`: свой адрес, псевдоним и Send As проходят; чужой `MAIL FROM` — `553`; чужой
  `From` и `Sender` при своём конверте — `550`; внешний домен — отказ; сеанс submission целиком; smtp-in не затронут.
