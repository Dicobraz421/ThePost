# DAV-23: `calendar-user-address-set` — псевдонимы учётной записи

lane: dav
deps: [CTR-53, DAV-05]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md#Metadata]
checks: [nextest]
manual: no

Хвост волны dav-02, PR #427.

## Сделать
- `props.rs`: `calendar-user-address-set` принципала — `mailto:` для каждого адреса из `AccountStore::addresses`
  (CTR-53), основной первым, затем ссылка на принципала. Сейчас — только адрес входа.
- `AccountStore` подключается построителем сервера (по образцу `Jmap::with_dav`), `Deps` не меняется; без подключения —
  прежнее поведение. Вызов в узле — APP-38.
- Эти же адреса — «свои» при разборе участников в планировании (DAV-13…16), если волна dav-04 уже влита.

## Приёмка
- Тест и снимок `PROPFIND`: учётная запись с псевдонимом — два `mailto:`; без `AccountStore` — как раньше.
