# KIT-08: фейк ящика — системные папки только по роли

lane: testkit
deps: [KIT-07]
class: A
files: [crates/tp-testkit/src/*]
read: []
checks: [nextest]
manual: no

Решение владельца #205 — как META-42 для in-memory реализации; conformance-тест на совпадение имени.
