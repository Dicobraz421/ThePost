# SMTP-18: обучение антиспама по перемещению в «Спам»

lane: smtp
deps: [SMTP-05]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S4.md#4.1, docs/spec/S2.md#2.5]
checks: [nextest]
manual: no

## Сделать
- Подписчик событий перемещения в папку «Спам» и из неё: письмо — в Rspamd controller (learn_spam, learn_ham) по HTTPS; пароль контроллера — из хранилища секретов; выключено по умолчанию.

## Приёмка
- Тест с подменным контроллером: перемещение вызывает обучение один раз.
