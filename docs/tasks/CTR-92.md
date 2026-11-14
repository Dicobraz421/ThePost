# CTR-92: матрица «кто — что» приёма iTIP — тип отправителя вместо источника

lane: contracts
deps: [CTR-47]
class: B
files: [crates/tp-contracts/src/schedule.rs, crates/tp-testkit/src/schedule.rs, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md#123]
checks: [nextest]
manual: no

## Сделать
- Решение 123 «Снаружи — только почта»: все решения «применять, бронировать, отвечать» — в одном месте,
  `schedule::inbound_allowed(SenderKind, InboundAction) -> bool`, чтобы потом подставить настройку администратора
  (политика или Sieve) без переделки вызывающих.
- `SenderKind::{Own, Partner, Neighbor, External}` — тип отправителя относительно организации получателя; заменяет
  `ItipSource::{Internal, Imip}` (поле `ItipMessage::sender` вместо `source`): «откуда пришло» не отвечает на вопрос
  «что можно» — письмо через smtp-in с адресом своего домена и письмо соседней организации установки шли одним
  `Imip`.
- `InboundAction::{Invite, Reply, Cancel, Other, Booking}` и `InboundAction::of(method, resource)` — клетки матрицы.
- Значения по умолчанию — по правилу владельца (решение 123): своей организации (вошедшему) — всё; извне (партнёр,
  соседняя организация, внешний) — `Reply` и `Cancel` к существующему событию, как у Exchange и Google (RFC 5546 §6.1,
  RFC 6638 §11.2); `Invite`, `Other`, `Booking` извне — нет (автодобавление приглашений партнёра — будущая
  настройка).

## Приёмка
- Тест матрицы и `InboundAction::of`; CHANGELOG в CONTRACTS.md.
