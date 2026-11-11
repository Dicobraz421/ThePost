//! SRS: туда и обратно, цепочка SRS1, подделка, срок, ротация ключа; HMAC — по вектору RFC 4231.

use time::Duration;

use super::*;

fn srs() -> Srs {
    Srs::new("fwd.example.org", vec![b"secret-key".to_vec()], 21)
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time")
}

#[test]
fn hmac_rfc4231() {
    let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn round_trip() {
    let s = srs();
    let fwd = s.forward("Alice@Example.NET", now()).expect("srs");
    assert!(fwd.starts_with("SRS0="), "{fwd}");
    assert!(fwd.ends_with("@fwd.example.org"), "{fwd}");
    assert!(s.is_srs(&fwd));
    assert_eq!(s.reverse(&fwd, now()).as_deref(), Ok("Alice@Example.NET"));
    // Регистр подписи и домена не важен (MTA меняют регистр адреса).
    assert_eq!(
        s.reverse(&fwd.to_lowercase(), now() + Duration::days(3))
            .map(|a| a.to_lowercase())
            .as_deref(),
        Ok("alice@example.net")
    );
    // Локальная часть с `=` — без потерь.
    let odd = s.forward("a=b@example.net", now()).expect("srs");
    assert_eq!(s.reverse(&odd, now()).as_deref(), Ok("a=b@example.net"));
}

#[test]
fn not_rewritten() {
    let s = srs();
    assert_eq!(s.forward("", now()), None);
    assert_eq!(s.forward("postmaster", now()), None);
    assert_eq!(s.forward("bob@fwd.example.org", now()), None);
    assert!(!s.is_srs("alice@example.net"));
    assert_eq!(
        s.reverse("alice@fwd.example.org", now()),
        Err(SrsError::NotSrs)
    );
}

#[test]
fn forged_and_expired() {
    let s = srs();
    let fwd = s.forward("alice@example.net", now()).expect("srs");
    // Подменён исходный адрес.
    let forged = fwd.replace("=alice@", "=mallory@");
    assert_eq!(s.reverse(&forged, now()), Err(SrsError::BadHash));
    // Чужой ключ.
    let other = Srs::new("fwd.example.org", vec![b"other".to_vec()], 21);
    assert_eq!(other.reverse(&fwd, now()), Err(SrsError::BadHash));
    // Срок: 21 день — действует, 22 — истёк.
    assert!(s.reverse(&fwd, now() + Duration::days(21)).is_ok());
    assert_eq!(
        s.reverse(&fwd, now() + Duration::days(22)),
        Err(SrsError::Expired)
    );
    // Чужой домен SRS.
    let foreign = fwd.replace("@fwd.example.org", "@other.example");
    assert_eq!(s.reverse(&foreign, now()), Err(SrsError::NotSrs));
}

#[test]
fn rotation_keeps_old_addresses() {
    let old = srs();
    let fwd = old.forward("alice@example.net", now()).expect("srs");
    let rotated = Srs::new(
        "fwd.example.org",
        vec![b"new-key".to_vec(), b"secret-key".to_vec()],
        21,
    );
    assert_eq!(
        rotated.reverse(&fwd, now()).as_deref(),
        Ok("alice@example.net")
    );
}

#[test]
fn srs1_chain() {
    // Первый узел переписал адрес, второй (мы) — пересылает дальше.
    let first = Srs::new("first.example", vec![b"k1".to_vec()], 21);
    let srs0 = first.forward("alice@example.net", now()).expect("srs0");
    let ours = srs();
    let srs1 = ours.forward(&srs0, now()).expect("srs1");
    assert!(srs1.starts_with("SRS1="), "{srs1}");
    assert!(srs1.contains("=first.example=="), "{srs1}");
    // Третий узел пересылает дальше: первый узел сохраняется.
    let third = Srs::new("third.example", vec![b"k3".to_vec()], 21);
    let again = third.forward(&srs1, now()).expect("srs1 again");
    assert!(again.contains("=first.example=="), "{again}");
    assert!(again.ends_with("@third.example"));
    // Обратно: SRS1 → SRS0 первого узла → исходный адрес.
    let back = ours.reverse(&srs1, now()).expect("srs1 reverse");
    assert_eq!(back, srs0);
    assert_eq!(
        first.reverse(&back, now()).as_deref(),
        Ok("alice@example.net")
    );
    let forged = srs1.replace("=first.example==", "=evil.example==");
    assert_eq!(ours.reverse(&forged, now()), Err(SrsError::BadHash));
}
