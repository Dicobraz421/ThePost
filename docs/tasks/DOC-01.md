# DOC-01: docs/PREREQUISITES.md — требования и тонкости настройки окружения

lane: docs
deps: []
class: A
files: [docs/PREREQUISITES.md, README.md]
read: [docs/spec/S8.md, docs/spec/APP-Z.md, docs/INSTALL.md, deploy/README.md, docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

## Сделать
- Один документ для администратора: всё, что должно быть верно в окружении до установки, с проверкой каждого пункта
  (команда или строка `thepost doctor`) и последствием нарушения. Разделы: ОС и ядро (версии, Landlock, seccomp,
  cgroup), железо по профилям, сеть и порты (в том числе исходящий 25), DNS и PTR, время (NTP), TLS и сертификаты,
  PostgreSQL (версия, кодировка UTF8 и `LC_CTYPE` UTF-8 — решение 23, параметры, расширения, TLS до СУБД), OpenBao
  (версия, политики, TLS), хранилище писем (ФС, место, S3), Rspamd и антивирус, лимиты (ulimit, файлы), учётная
  запись службы и права, резервное копирование (APP-Z), ограничения и известные тонкости.
- Источники — спецификация (S8, APP-Z), `deploy/README.md`, роли Ansible, проверки `thepost doctor`
  (`crates/thepost/src/doctor*.rs`), решения в `docs/decisions/`. Ничего не выдумывать: где значения нет — так и писать.
- Ссылка из `README.md` (раздел «Установка» и «Ещё»). `docs/INSTALL.md` не трогать — ссылку оттуда добавит DEP-15.

## Приёмка
- Каждый пункт: требование, как проверить, что будет при нарушении; ссылки на источник.
