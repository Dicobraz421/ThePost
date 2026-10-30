# OpenBao профилей tls и smb: хранилище Raft, только TLS, автоматическое распечатывание.
# Ключ распечатывания — в файле (seal "static", том openbao-seal): на стенде нет TPM/PKCS#11 и KMS;
# thepost doctor предупреждает, что секреты защищены слабее. На сервере — seal "pkcs11" или KMS (deploy/README.md).

storage "raft" {
  path    = "/openbao/data"
  node_id = "openbao-1"
}

listener "tcp" {
  address         = "0.0.0.0:8200"
  cluster_address = "0.0.0.0:8201"
  tls_cert_file   = "/openbao/tls/server.pem"
  tls_key_file    = "/openbao/tls/server.key"
  tls_min_version = "tls12"
}

seal "static" {
  current_key_id = "compose-1"
  current_key    = "file:///openbao/seal/unseal.key"
}

api_addr      = "https://openbao-tls:8200"
cluster_addr  = "https://openbao-tls:8201"
disable_mlock = true
ui            = false
