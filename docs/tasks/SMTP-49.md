# SMTP-49: тип отправителя письма с iTIP — по пути и домену `From`

lane: smtp
deps: [CTR-92, SMTP-33, SMTP-36]
class: B
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#123, docs/tasks/SMTP-33.md, docs/tasks/SMTP-36.md]
checks: [nextest]
manual: no

## Сделать
- `Delivery::apply_itip` передаёт приёму iTIP тип отправителя (`SenderKind`, CTR-92), а решение «что можно» — матрице
  `inbound_allowed` у приёма (DAV-43). Доверие к `From` — как было: smtp-in — DMARC, без `SENT-BY` извне; внутренний
  путь — `From` принадлежит вошедшей учётной записи.
- Своя организация (`Own`) — по входу, а не по домену: внутренний путь (submission со входом, `Delivery::submit`).
  Письмо через smtp-in (Edge, шлюз) — по домену `From` (ближайший принятый домен кластера: домен или родитель,
  поддомен принадлежит организации домена): свой домен — `External` (DMARC проходит и сторонний сервис домена),
  домен-партнёр — `Partner`, принятый домен другой организации установки — `Neighbor`, иначе — `External`.
- Домены-партнёры — `Delivery::with_partners(Arc<dyn PartnerDomains>)`. Источника списка пока нет (политика
  организации, META-34 — todo): узел его не подключает, партнёров нет.

## Приёмка
- Тесты: smtp-in — без партнёров `External`, партнёр `Partner`, свой домен `External` (и в списке партнёров),
  другая организация установки `Neighbor` или `Partner`; организация адреса — по ближайшему принятому домену;
  внутренний путь — `Own` при любом домене `From` (Send As на адрес другой организации и внешний).
