//! Скользящее окно лимита отправки на заданном времени.

use super::*;

#[test]
fn hourly_limit_exceeded_then_window_slides() {
    let l = SendLimits::new();
    let t0 = Instant::now();
    assert!(l.allow_message_at(2, "alice", t0));
    l.record_at(100, "alice", t0);
    l.record_at(100, "alice", t0);
    // Превышение: третье письмо за час.
    assert!(!l.allow_message_at(2, "alice", t0 + Duration::from_secs(60)));
    // Больший лимит другой политики — разрешено; другая учётная запись не затронута.
    assert!(l.allow_message_at(3, "alice", t0));
    assert!(l.allow_message_at(2, "bob", t0));
    // Через час прошлый счёт ещё почти целиком в оценке, через полтора — наполовину.
    assert!(!l.allow_message_at(2, "alice", t0 + HOUR + Duration::from_secs(60)));
    assert!(l.allow_message_at(2, "alice", t0 + HOUR + HOUR / 2));
    // Через два часа счёт обнулён.
    assert!(l.allow_message_at(1, "alice", t0 + 2 * HOUR));
}

#[test]
fn tracking_is_bounded() {
    let l = SendLimits::new();
    let t0 = Instant::now();
    l.record_at(1, "alice", t0);
    l.record_at(1, "bob", t0);
    assert_eq!(l.len(), 1);
    // Давно не отправлявшие освобождают место.
    l.record_at(1, "bob", t0 + 2 * HOUR);
    assert_eq!(l.len(), 1);
    assert!(!l.allow_message_at(1, "bob", t0 + 2 * HOUR));
}
