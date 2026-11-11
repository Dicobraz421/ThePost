# DEP-18: thepost_pki — перевыпуск внутреннего сертификата до конца срока

lane: deploy
deps: []
class: A
files: [deploy/ansible/roles/thepost_pki/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

## Сделать
- При прогоне плейбука: если до `not_after` сертификата узла, PostgreSQL или OpenBao меньше
  `thepost_pki_renew_before` (по умолчанию 90 сут) — перевыпуск тем же УЦ и перезапуск/перечитывание служб (решение 27).
- Срок УЦ — отдельно; при истечении УЦ — понятная ошибка с инструкцией.

## Приёмка
- С `thepost_pki_cert_not_after: "+1d"` повторный прогон перевыпускает; с обычным сроком — без изменений.
