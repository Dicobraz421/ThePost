# Политика узла thepost — общая для compose, Helm и Ansible:
# чтение секретов KV v2 `thepost`, продление своего токена и аренд; запись — только ключ TLS встроенного ACME.
path "thepost/data/*" {
  capabilities = ["read"]
}

# Ключ сертификата встроенного клиента ACME (tls.key: { secret: "thepost/tls#key" }, PLT-29): узел пишет новый ключ
# при выпуске и продлении (решение 101).
path "thepost/data/tls" {
  capabilities = ["create", "update", "read"]
}

# Пароль учётной записи PostgreSQL «только чтение» (thepost_readonly) — администратору и отчётам, не узлу (DEP-08).
path "thepost/data/db-readonly" {
  capabilities = ["deny"]
}

path "thepost/metadata/*" {
  capabilities = ["read", "list"]
}

path "auth/token/lookup-self" {
  capabilities = ["read"]
}

path "auth/token/renew-self" {
  capabilities = ["update"]
}

path "sys/leases/renew" {
  capabilities = ["update"]
}
