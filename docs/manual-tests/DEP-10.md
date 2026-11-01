# DEP-10: ручная проверка «малого Enterprise»

1. Развернуть эталон Ansible: 2 узла `mailbox`, pg_auto_failover (primary, standby, монитор), SeaweedFS на трёх ВМ данных.
2. Под нагрузкой imaptest и smtp-source выключить узел `mailbox`: клиенты переподключаются ко второму узлу, письма не теряются.
3. Выключить ВМ с primary PostgreSQL: переключение ≤ 30 с, узлы находят новый primary, на время переключения SMTP отвечает 4xx.
4. Выключить одну ВМ данных SeaweedFS: чтение и запись BLOB продолжаются.
5. `thepost verify` после возврата всех ВМ — расхождений нет.
