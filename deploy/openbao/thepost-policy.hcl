# Политика узла thepost — общая для compose, Helm и Ansible:
# только чтение секретов KV v2 `thepost`, продление своего токена и аренд.
path "thepost/data/*" {
  capabilities = ["read"]
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
