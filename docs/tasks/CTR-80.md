# CTR-80: заголовки скрытого письма корзины для администратора

lane: contracts
deps: [CTR-35]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/tasks/MS-19.md, docs/tasks/ADM-19.md]
checks: [nextest]
manual: no

## Сделать
- Контракт чтения заголовков (`Subject`, `From`, дата, размер) скрытого письма корзины (`trash/hidden`) без снятия
  скрытия: `TrashItem` получает ссылку на скелет письма или `TrashStore` — метод чтения заголовков по элементу корзины.
  Пользователю письмо остаётся невидимым. Реализация в памяти и общий набор тестов; потребители — в том же PR.
- Нужна для MS-19 и экрана корзины в консоли (отчёт admin-13, #487; агент mailstore-12: `MessageStore::get` скрытое
  письмо не отдаёт, в `TrashItem` нет ключа скелета).

## Приёмка
- Набор testkit: заголовки читаются, `MessageStore::get` по-прежнему не отдаёт письмо.
