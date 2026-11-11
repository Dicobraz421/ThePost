# syntax=docker/dockerfile:1.7
# OCI-образ The Post: один бинарный файл, профиль — по config.yaml (SRS 1.2).
# Сборка из корня репозитория: docker build -f deploy/docker/Dockerfile -t thepost .

ARG RUST_VERSION=1.98

FROM rust:${RUST_VERSION}-bookworm AS build
ENV CARGO_TERM_COLOR=never
WORKDIR /src
COPY . .
# Тулчейн образа вместо channel из rust-toolchain.toml: сборка воспроизводима и без загрузки rustup.
# Имя — полное, из `rustup default` (1.97.1-x86_64-…): короткое `1.97` rustup не сопоставляет с установленным
# 1.97.1 и идёт качать канал.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    RUSTUP_TOOLCHAIN="$(rustup default | cut -d' ' -f1)" cargo build --release --locked -p thepost \
 && install -D -m 0755 target/release/thepost /out/bin/thepost \
 && install -d -m 0750 /out/state /out/etc \
 && install -d -m 0700 /out/state/blob

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/bin/thepost /usr/local/bin/thepost
# Каталоги создаются в сборке: в distroless нет оболочки, nonroot = 65532. Каталог BLOB (blob.url эталона smb)
# нужен до наложения Landlock — пустой том копирует его из образа (до CMP-02 узел без него не запускается).
COPY --from=build --chown=65532:65532 /out/state /var/lib/thepost
COPY --from=build --chown=65532:65532 /out/etc /etc/thepost
USER 65532:65532
WORKDIR /var/lib/thepost
VOLUME ["/var/lib/thepost"]
# SMTP, submission (465/587), IMAP (143/993), HTTP(S): JMAP, DAV, Admin API.
# Порты <1024 доступны nonroot: Docker задаёт net.ipv4.ip_unprivileged_port_start=0.
EXPOSE 25 465 587 143 993 80 443
ENTRYPOINT ["/usr/local/bin/thepost"]
CMD ["serve", "--config", "/etc/thepost/config.yaml"]
