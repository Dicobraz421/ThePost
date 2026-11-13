-- Действие «поиск» в журнале аудита ящика (CTR-81, решение 114; CONTRACTS.md#MailboxAudit): поиск по тексту писем в
-- чужом ящике; строка запроса — в detail, найденные письма не хранятся.

ALTER TABLE mailbox_audit DROP CONSTRAINT mailbox_audit_action_check;

ALTER TABLE mailbox_audit
    ADD CONSTRAINT mailbox_audit_action_check CHECK (action IN ('login', 'read', 'search', 'move', 'delete', 'send',
                                                                 'send_as', 'send_on_behalf', 'change_rights',
                                                                 'change_rules', 'change_forwarding'));
