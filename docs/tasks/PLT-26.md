# PLT-26: внутренние сертификаты движком PKI OpenBao

lane: platform
deps: [PLT-24]
class: A
files: [crates/tp-platform/src/*, deploy/ansible/roles/openbao/*, deploy/ansible/roles/thepost_pki/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Выпуск и продление внутренних сертификатов core-компонентов (узлы, СУБД, OpenBao, SeaweedFS) движком PKI
  OpenBao коротким сроком; слушатели клиентов и интеграции — можно внешний УЦ заказчика (решение 64).

## Приёмка
- Тест: сертификат продлевается до конца срока без перезапуска.
