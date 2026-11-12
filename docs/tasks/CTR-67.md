# CTR-67: политика организации для SMTP — потребители полей

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Policy]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 5; решение 97.

## Сделать
- `Policy`: у `send_per_hour`, `max_recipients`, `dkim_selector`, `milter_profile` — описание потребителя (SMTP, SMTP-40)
  и умолчания уровня организации; `retention` — исполнитель срока хранения писем (модуль «Соответствие требованиям» или
  maintenance) или удалить из базовой политики. testkit `PolicyResolver` с этими полями; CHANGELOG.

## Приёмка
- CI зелёный; набор разрешения политики покрывает новые поля.
