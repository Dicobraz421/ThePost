# DEP-13: стенд SMB — замеры и отчёт по критериям Д.7

lane: deploy
deps: [DEP-12]
class: A
files: [deploy/stand/*, docs/manual-tests/DEP-13.md]
read: [docs/spec/APP-D.md#Д.7, deploy/stand/roles/stand_generator/files/run-scenario.sh]
checks: [none]
manual: yes

## Сделать
- Зонды генератора на время прогона (готовые инструменты: k6 для JMAP и WebSocket, Python stdlib — `imaplib`,
  `smtplib`): время до ответа `250` на 25; внутренняя доставка — от отправки на 587 до появления в ящике (IMAP IDLE);
  сигнал — от изменения флага до push JMAP / `EXISTS` IDLE на другом соединении; `SELECT` ящика 10 000 писем;
  JMAP `Email/query` и `Email/get`; полнотекстовый поиск; задержка индексации (уникальное слово → находится поиском).
- Память простаивающего соединения: N соединений IMAP IDLE и N push, рост RSS узла (`/metrics`) / N.
- `criteria.py`: из журналов прогона — таблица Д.7 для SMB (показатель, порог, p95/p99, «прошёл» / «не прошёл» /
  «не измерялось» с причиной) в `criteria.md` и `criteria.json`; код выхода ≠ 0, если есть «не прошёл».
- `run-scenario.sh` в конце прогона вызывает `criteria.py`; итог — в выводе Ansible.

## Приёмка
- Короткий прогон на уменьшенном стенде даёт `criteria.md` со всеми строками Д.7 профиля SMB; строки, которые
  к SMB не относятся (шарды, primary HA, 1 млн писем), — «не относится к SMB».
