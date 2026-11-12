# KIT-10: корпус vCard: проверка разбором и новые образцы

lane: testkit
deps: [DEPS-11]
class: A
files: [crates/tp-testkit/*]
read: [docs/tasks/KIT-09.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 41: testkit-06, PR #482.

## Сделать
- Тест: каждый образец корпуса разбирается `calcard` без ошибок.
- Образцы: файл с несколькими карточками, vCard 2.1/3.0 в 8-битной кодировке (`CHARSET=windows-1251`, quoted-printable).

## Приёмка
- Тест корпуса зелёный; новые образцы описаны в каталоге корпуса.
