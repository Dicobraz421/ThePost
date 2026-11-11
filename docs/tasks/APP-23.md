# APP-23: подпись профиля .mobileconfig в узле

lane: app
deps: [PLT-23, JMAP-22]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: docs/manual-tests/JMAP-22.md

Решение владельца 2026-11-11, п. 20.

## Сделать
- `TlsProfileSigner` (`tp_jmap::ProfileSigner`): ключ и цепочка — из tp-platform (PLT-23); RSA → `RsaPkcs1Sha256`,
  ECDSA P-256/P-384 → `EcdsaP256Sha256`/`EcdsaP384Sha384`; Ed25519 и отсутствие TLS — без подписи.
- `Services::jmap`: `Jmap::with_profile_signer(Some(..))`.

## Приёмка
- Тест: профиль, отданный узлом, подписан текущим сертификатом и проверяется цепочкой; после перечитывания — новым.
