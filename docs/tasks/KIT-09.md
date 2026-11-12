# KIT-09: корпус vCard

lane: testkit
deps: []
class: A
files: [crates/tp-testkit/src/*, crates/tp-testkit/corpus/*]
read: [docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Хвост волны jmap-16, PR #428.

## Сделать
- Каталог `corpus/` с файлами `.vcf` и доступ из `corpus.rs` (как к письмам): vCard 4.0 с кириллицей и `X-`свойством,
  организация (`KIND:org`), vCard 3.0 с группой `item1.`, группа контактов (`KIND:group`, `MEMBER`), фото, несколько
  адресов и телефонов с типами, экранирование и перенос длинных строк, выгрузки распространённых клиентов.
- Только вымышленные данные; у каждого файла — что он проверяет (комментарий в `corpus.rs`).

## Приёмка
- Тест `tp-testkit`: каждый файл корпуса разбирается `calcard` без ошибок; потребители — JMAP-34 и тесты CardDAV.
