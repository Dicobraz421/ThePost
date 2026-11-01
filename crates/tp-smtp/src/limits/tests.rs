//! Скользящее окно лимитов отправки на заданном времени.

use super::*;

fn cfg(messages: u32, rcpts: u32) -> Limits {
    Limits {
        messages_per_hour: messages,
        recipients_per_hour: rcpts,
        ..Limits::default()
    }
}

#[test]
fn hourly_limit_exceeded_then_window_slides() {
    let l = SendLimits::new();
    let cfg = cfg(2, 3);
    let t0 = Instant::now();
    assert!(l.allow_message_at(&cfg, "alice", t0));
    assert!(l.allow_rcpts_at(&cfg, "alice", 3, t0));
    assert!(!l.allow_rcpts_at(&cfg, "alice", 4, t0));
    l.record_at(&cfg, "alice", 2, t0);
    l.record_at(&cfg, "alice", 1, t0);
    // Превышение: третье письмо и четвёртый получатель за час.
    assert!(!l.allow_message_at(&cfg, "alice", t0 + Duration::from_secs(60)));
    assert!(!l.allow_rcpts_at(&cfg, "alice", 1, t0));
    // Другая учётная запись не затронута.
    assert!(l.allow_message_at(&cfg, "bob", t0));
    // Через час прошлый счёт ещё почти целиком в оценке, через полтора — наполовину.
    assert!(!l.allow_message_at(&cfg, "alice", t0 + HOUR + Duration::from_secs(60)));
    assert!(l.allow_message_at(&cfg, "alice", t0 + HOUR + HOUR / 2));
    // Через два часа счёт обнулён.
    assert!(l.allow_rcpts_at(&cfg, "alice", 3, t0 + 2 * HOUR));
}

#[test]
fn zero_disables_and_tracking_is_bounded() {
    let l = SendLimits::new();
    let t0 = Instant::now();
    let off = cfg(0, 0);
    l.record_at(&off, "alice", 1000, t0);
    assert!(l.is_empty());
    assert!(l.allow_message_at(&off, "alice", t0));
    assert!(l.allow_rcpts_at(&off, "alice", 1_000_000, t0));
    let small = Limits {
        max_tracked_accounts: 1,
        ..cfg(1, 1)
    };
    l.record_at(&small, "alice", 1, t0);
    l.record_at(&small, "bob", 1, t0);
    assert_eq!(l.len(), 1);
    // Давно не отправлявшие освобождают место.
    l.record_at(&small, "bob", 1, t0 + 2 * HOUR);
    assert_eq!(l.len(), 1);
    assert!(!l.allow_message_at(&small, "bob", t0 + 2 * HOUR));
}
