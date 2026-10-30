# syntax=docker/dockerfile:1.7
# OCI-образ The Post: один бинарный файл, профиль — по config.yaml (SRS 1.2).
# Сборка из корня репозитория: docker build -f deploy/docker/Dockerfile -t thepost .

ARG RUST_VERSION=1.97

FROM rust:${RUST_VERSION}-bookworm AS build
ARG RUST_VERSION
# Тулчейн образа вместо channel из rust-toolchain.toml: сборка воспроизводима и без загрузки rustup.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_TERM_COLOR=never
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p thepost \
 && install -D -m 0755 target/release/thepost /out/bin/thepost \
 && install -d -m 0750 /out/state /out/etc

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/bin/thepost /usr/local/bin/thepost
# Каталоги создаются в сборке: в distroless нет оболочки, nonroot = 65532.
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
