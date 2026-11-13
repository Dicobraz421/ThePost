# META-80: почтовые контакты в каталоге PostgreSQL (`Directory::contacts_since`)

lane: meta
deps: [META-65]
class: A
files: [crates/tp-meta/src/directory.rs, crates/tp-meta/tests/pg_directory.rs]
read: [docs/CONTRACTS.md#Directory]
checks: [nextest]
manual: no

Приёмка CMP-10, ошибка 1 (шаг 3.2): почтовый контакт, созданный через Admin API, не попадает в книгу «Организация»
без LDAP. SRS 4.5: почтовые контакты «входят в GAL и в состав групп». Каталог `PgMeta` контактов не отдавал.

## Сделать
- `PgMeta::contacts_since`: почтовые контакты организации (`mail_contacts`) без скрытых из GAL, порциями и с курсором,
  как у учётных записей. `external_id` — `ContactId`.
- `DirGroup::members` — также `external_id` видимых контактов группы (`MailContact::groups`), по контракту.
- `gal_attributes` — также имена атрибутов видимых контактов.
- Чтение контактов при построении книги (`tp_dav::gal::sync`) — полоса dav, в этой волне не делать.

## Приёмка
- Тест (PostgreSQL): контакт из Admin API (`ContactStore::put_contact`) виден в `contacts_since` и в участниках группы,
  с `hidden_from_gal` — нет; скрытие меняет версию каталога.
