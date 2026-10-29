# AUTH-05: Kerberos GSSAPI

lane: auth
deps: [AUTH-04]
class: A
files: [crates/tp-auth/src/gssapi.rs]
read: [docs/spec/S5.md#5.1]
checks: [nextest (feature gssapi — вручную)]
manual: yes

## Сделать
- Механизм GSSAPI под feature gssapi (libgssapi, системная MIT Kerberos), keytab из конфигурации.

## Приёмка
- Сборка с feature; ручная проверка с FreeIPA — docs/manual-tests/AUTH-05.md.
