-- Transactional outbox for user notifications.
--
-- Notification intents are written here in the same transaction as the
-- domain mutation that produced them. A worker then claims pending rows,
-- delivers them (inserting into `notifications`), and marks them sent.
-- Delivery failures are retried with exponential backoff until a terminal
-- failure state is reached.

CREATE TABLE IF NOT EXISTS notification_outbox (
    id                BIGSERIALL PRIMARY KEY,
    -- Delivery payload: the notification intent to persist into `notifications`.
    user_address      TEXT        NOT NULL,
    notif_type         TEXT        NOT NULL,
    title              TEXT        NOT NULL,
    message            TEXT        NOT NULL,
    pool_id            BIGINT      NOT NULL,
    -- Lifecycle state.
    status              TEXT        NOT NULL DEFAULT 'pending',
    attempts            INTEGER     NOT NULL DEFAULT 0,
    max_attempts        INTEGER     NOT NULL DEFAULT 5,
    next_attempt_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    last_error         TEXT,
    claimed_at         TIMESTAMPTZ,
    claimed_by         TEXT,
    created_at          TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    CONSTRAINT notification_outbox_status_check
        CHECK (status IN ('pending', 'claimed', 'sent', 'failed'))
);

-- Worker claiming scans for due pending rows; this index keeps that
-- scan cheap as the table grows.
CREATE INDEX IF NOT EXISTS idx_notification_outbox_due
    ON notification_outbox (next_attempt_at)
    WHERE status = 'pending';

-- Supports recovery of rows claimed by a worker that died before
-- completing delivery.
CREATE INDEX IF NOT EXISTS idx_notification_outbox_claimed
    ON notification_outbox (claimed_at)
    WHERE status = 'claimed';
