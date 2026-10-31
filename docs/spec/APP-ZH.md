# Приложение Ж. Соответствие параметров Postfix

Приложение применяется при миграции с Postfix: для типовых параметров main.cf указано, чем они покрываются (правило выбора функций — п. 4.1).

Статусы: встроено — функция ядра; правилом — правило потока почты (п. 4.1); Milter — внешний фильтр; внешний MTA — шлюз или промежуточный узел на периметре (п. 2.6); не требуется — функция не нужна в модели продукта; не поддерживается — функции нет.

**Таблица: Соответствие параметров Postfix**

| Параметры Postfix | Статус | Реализация в The Post |
|---|---|---|
| myhostname, mydomain, myorigin, inet_interfaces, inet_protocols, smtpd_banner, smtpd_*_timeout | Встроено | Параметры узла в config.yaml |
| mydestination, virtual_mailbox_domains, virtual_alias_domains | Встроено | Полномочные принятые домены |
| relay_domains, relay_recipient_maps | Встроено | Принятые домены с внутренней ретрансляцией |
| local_recipient_maps, virtual_mailbox_maps | Встроено | Проверка получателя по каталогу в RCPT TO |
| virtual_alias_maps, alias_maps | Встроено | Псевдонимы и группы рассылки в каталоге |
| recipient_delimiter, luser_relay | Встроено | Подадреса, catch-all |
| canonical_maps, sender_canonical_maps, recipient_canonical_maps, masquerade_domains, smtp_generic_maps | Встроено | Переписывание адресов |
| mynetworks, permit_mynetworks | Встроено | Доверенный источник «приложение» со списком отправителей; ретрансляция для сети без списка отправителей — внешний MTA |
| smtpd_authorized_xclient_hosts, smtpd_authorized_xforward_hosts | Встроено | Доверенный источник «шлюз» |
| smtpd_upstream_proxy_protocol | Встроено | PROXY protocol v2 (п. 6.1) |
| smtpd_client_restrictions, smtpd_helo_restrictions, smtpd_sender_restrictions, smtpd_recipient_restrictions, smtpd_relay_restrictions | Встроено, Milter | Фиксированная цепочка проверок ядра; проверки HELO, reject_unknown_* — Rspamd через Milter |
| reject_rbl_client, reject_rhsbl_*, postscreen_dnsbl_sites | Milter | Rspamd (DNSBL, DNSWL) |
| postscreen_greet_action, postscreen_pipelining_enable, postscreen_cache_* | Встроено | Экран соединений |
| smtpd_client_connection_rate_limit, smtpd_client_message_rate_limit | Встроено | Лимиты частоты по адресу и учётной записи |
| smtpd_sasl_*, smtpd_tls_auth_only | Встроено | submission (587, 465) с SASL |
| smtpd_sender_login_maps, reject_sender_login_mismatch | Встроено | Право отправки от имени (делегирование) |
| smtpd_tls_*, smtp_tls_* | Встроено | TLS (п. 6.2) |
| smtp_tls_policy_maps, smtp_tls_security_level | Встроено | Политики доменов назначения, MTA-STS, DANE |
| relayhost | Встроено | Политика для всех внешних доменов |
| transport_maps (по доменам) | Встроено | Политика домена назначения (маршрут) |
| transport_maps (регулярные выражения, транспорты pipe, lmtp, uucp) | Внешний MTA | Промежуточный узел для этих доменов |
| sender_dependent_relayhost_maps, sender_dependent_default_transport_maps | Правилом | Действие «маршрут по политике домена назначения» |
| smtp_bind_address, smtp_bind_address6 | Встроено | egress-IP политики домена |
| default_destination_concurrency_limit, initial_destination_concurrency, *_destination_rate_delay | Встроено | Планировщик исходящей очереди |
| maximal_queue_lifetime, bounce_queue_lifetime, minimal_backoff_time, maximal_backoff_time | Встроено | Параметры smtp-out в config.yaml |
| message_size_limit | Встроено | Лимит SIZE в config.yaml |
| mailbox_size_limit, virtual_mailbox_limit | Встроено | Квота ящика |
| smtpd_milters, non_smtpd_milters, milter_default_action, milter_*_timeout | Встроено | Цепочка Milter, политика при недоступности фильтра |
| content_filter, receive_override_options (amavisd-new) | Milter, внешний MTA | amavisd-milter; фильтр после очереди — шлюз перед smtp-in |
| header_checks, mime_header_checks, body_checks, smtp_header_checks | Правилом | Условия по заголовкам и тексту; сложная обработка — Milter |
| always_bcc, sender_bcc_maps, recipient_bcc_maps | Правилом | Действие «копия»; журналирование — модуль «Соответствие требованиям» |
| OpenDKIM, OpenDMARC (Milter) | Встроено | DKIM, DMARC, ARC в ядре |
| postsrsd | Встроено | SRS и ARC при пересылке |
| mailbox_command, mailbox_transport, home_mailbox, .forward, procmail | Встроено | Sieve (фильтры, перенаправление); внешние программы не запускаются |
| address_verify_*, reject_unverified_recipient | Не требуется | Получатели — в каталоге; продукт отвечает на проверку шлюза отказом в RCPT TO |
| master.cf, *_process_limit, несколько экземпляров | Не требуется | Роли и пресеты (п. 2.1), асинхронная среда выполнения |
| Таблицы поиска (hash, lmdb, regexp, ldap, pgsql, mysql, tcp, socketmap) | Не требуется | Данные — в каталоге и PostgreSQL |
| sendmail, postdrop, pickup, mailq | Не поддерживается | Приложения отправляют по submission или как доверенный источник |
| bounce_template_file, notify_classes | Не поддерживается | DSN по RFC 3464; оповещения администратору — мониторинг (п. 6.4) |
| VRFY, ETRN | Не поддерживается | Команды не реализованы |
