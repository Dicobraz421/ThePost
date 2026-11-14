# DOC-10: INSTALL — входящая почта через шлюз, доверие к `Authentication-Results`

lane: docs
deps: [SMTP-47]
class: A
files: [docs/INSTALL.md]
read: [docs/tasks/SMTP-20.md, crates/tp-smtp/src/authcheck.rs]
checks: []
manual: no

## Сделать
- Подраздел «Входящая почта через шлюз» в `docs/INSTALL.md` после «DNS домена»: при `authserv_id` и без
  XCLIENT/XFORWARD берутся результаты шлюза (`Authentication-Results` с этим authserv-id, иначе ARC с печатью домена
  шлюза); шлюз обязан выполнять RFC 8601 §5 (удалять чужие `Authentication-Results` со своим authserv-id), иначе
  подделка `dmarc=pass` и принятые как подлинные приглашения iMIP; authserv-id — домен шлюза или поддомен, ARC
  подписан ключом этого домена.
- Где задаётся: доверенный источник (`thepost admin transport source set --authserv-id`, Admin API).

## Приёмка
- Текст совпадает с поведением `tp_smtp::authcheck` после #792.
