# Политика узла thepost — общая для compose, Helm и Ansible:
# чтение секретов KV v2 `thepost`, запись — только ключа TLS слушателей; продление своего токена и аренд.
path "thepost/data/*" {
  capabilities = ["read"]
}

# Ключ TLS слушателей (`tls.key`, решение 100): встроенный ACME записывает выпущенный ключ новой версией (PLT-29).
path "thepost/data/tls" {
  capabilities = ["read", "create", "update"]
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
