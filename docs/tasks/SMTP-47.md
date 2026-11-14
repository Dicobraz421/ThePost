# SMTP-47: результаты шлюза — печать ARC домена шлюза и `header.from` письма

lane: smtp
deps: [SMTP-20, SMTP-33]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/tasks/SMTP-20.md]
checks: [nextest]
manual: no

Аудит приёма iMIP: `AuthCheck::gateway` верит результатам шлюза больше, чем они доказывают. При откате на ARC
authserv-id в `ARC-Authentication-Results` пишет кто угодно — звено с печатью своего домена проходит проверку
цепочки и выдаёт себя за шлюз. `dmarc=pass` засчитывается без сверки `header.from=` с `From` письма — результат о
другом письме (вписанный отправителем или пересланный) делает адрес `From` доверенным для iMIP (SMTP-33).

## Сделать
- Откат на ARC: печать последнего звена (`ARC-Seal`, `d=`) — домена шлюза: authserv-id — этот домен или его
  поддомен; домен верхнего уровня не годится. Иначе — `None` (обычные проверки).
- `dmarc_pass`: есть результат `dmarc` и каждый — `pass` с `header.from=` домена `From` письма (один домен у всех
  адресов `From`, как у DMARC в mail-auth). Без `header.from=` или при расхождении — не пройден.
- Doc-комментарий модуля: требование RFC 8601 §5 к шлюзу (удалять чужие `Authentication-Results` со своим
  authserv-id) — без него подделку, совпадающую с письмом, узел не отличит.
- Тесты: поддельный AR (другой домен, без `header.from=`, разные домены `From`), поддельный ARC (печать чужого
  домена), печать родительского домена шлюза.

## Приёмка
- `Authentication-Results: gw.example.org; dmarc=pass header.from=attacker.example` у письма `From: …@example.com` —
  `dmarc_pass = false`; с `header.from=example.com` — `true`.
- Звено ARC с authserv-id шлюза и печатью `d=attacker.example` — `None`; с печатью `d=example.org` у шлюза
  `gw.example.org` — результаты звена.
