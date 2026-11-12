//! Скользящее окно лимита отправки на заданном времени.

use super::*;

#[test]
fn hourly_limit_exceeded_then_window_slides() {
    let l = SendLimits::new();
    let t0 = Instant::now();
    assert!(l.allow_message_at(2, "alice", t0));
    l.record_at(100, "alice", 1, t0);
    l.record_at(100, "alice", 1, t0);
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
    l.record_at(1, "alice", 1, t0);
    l.record_at(1, "bob", 1, t0);
    assert_eq!(l.len(), 1);
    // Давно не отправлявшие освобождают место.
    l.record_at(1, "bob", 1, t0 + 2 * HOUR);
    assert_eq!(l.len(), 1);
    assert!(!l.allow_message_at(1, "bob", t0 + 2 * HOUR));
}

/// Адресаты в час (SMTP-43): сумма получателей принятых писем; письмо, с которым сумма превысила бы предел, не
/// принимается, писем в час это не касается.
#[test]
fn hourly_recipients_limit() {
    let l = SendLimits::new();
    let t0 = Instant::now();
    assert!(l.allow_rcpts_at(2000, "alice", 900, t0));
    l.record_at(100, "alice", 900, t0);
    assert!(l.allow_rcpts_at(2000, "alice", 900, t0));
    l.record_at(100, "alice", 900, t0);
    assert!(!l.allow_rcpts_at(2000, "alice", 900, t0));
    assert!(l.allow_rcpts_at(2000, "alice", 200, t0), "остаток предела");
    assert!(l.allow_message_at(3, "alice", t0), "писем — два из трёх");
    assert!(l.allow_rcpts_at(2000, "bob", 900, t0));
    // Через полтора часа прошлый счёт — наполовину.
    assert!(l.allow_rcpts_at(2000, "alice", 900, t0 + HOUR + HOUR / 2));
}
