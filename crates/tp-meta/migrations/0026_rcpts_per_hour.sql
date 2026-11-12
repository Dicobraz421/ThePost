-- Лимит отправки «адресатов в час» на уровне цепочки политик (CTR-79, решение 113; SRS 6.3): сумма получателей
-- писем учётной записи за скользящий час, как RecipientRateLimit в Exchange. NULL — не задано на этом уровне
-- (умолчание уровня организации — Policy::DEFAULT_RCPTS_PER_HOUR).
ALTER TABLE policies
    ADD COLUMN rcpts_per_hour bigint CHECK (rcpts_per_hour BETWEEN 0 AND 4294967295);
