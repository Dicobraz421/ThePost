# syntax=docker/dockerfile:1.7
# OCI-образ The Post: один бинарный файл, профиль — по config.yaml (SRS 1.2).
# Сборка из корня репозитория: docker build -f deploy/docker/Dockerfile -t thepost .
# Веб-консоль (SRS 5.2) вшита по умолчанию; без консоли и без Node — `--build-arg CONSOLE=0`.
# Вход по Kerberos (фича `gssapi`, решение 107, CI-12) — всегда, как в выпуске: узлу нужна libgssapi_krb5 (DEP-39).

ARG RUST_VERSION=1.98
# Node — как в CI (.github/actions/console, node-version).
ARG NODE_VERSION=26
ARG CONSOLE=1

# Сборка console/dist (DEPS-07). BuildKit собирает только стадию console-${CONSOLE}: при CONSOLE=0 Node не нужен.
FROM node:${NODE_VERSION}-bookworm-slim AS console-1
WORKDIR /console
COPY console/package.json console/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-fund --no-audit
COPY console/ ./
RUN npm run build && test -f dist/index.html && mv dist /dist

FROM rust:${RUST_VERSION}-bookworm AS console-0
RUN mkdir /dist

FROM console-${CONSOLE} AS console

FROM rust:${RUST_VERSION}-bookworm AS build
ARG CONSOLE
ENV CARGO_TERM_COLOR=never
# Фича gssapi: libgssapi-sys генерирует привязки bindgen'ом (libclang) по заголовкам MIT Kerberos (libkrb5-dev).
# Библиотеки запуска — libgssapi-krb5-2 и её зависимости, которых нет в distroless/cc (Debian 12, как и эта стадия):
# в /out/krb5/usr/lib/<триплет>, последняя стадия накладывает каталог на корень.
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends libclang-dev libkrb5-dev >/dev/null \
 && rm -rf /var/lib/apt/lists/* \
 && lib="/out/krb5/usr/lib/$(gcc -dumpmachine)" && mkdir -p "$lib" \
 && dpkg -L libgssapi-krb5-2 libkrb5-3 libk5crypto3 libkrb5support0 libcom-err2 libkeyutils1 \
      | grep -E '^(/usr)?/lib/[^/]+/lib[^/]+\.so\.[.0-9]+$' | xargs cp -a -t "$lib" \
 && test -e "$lib/libgssapi_krb5.so.2"
WORKDIR /src
COPY . .
COPY --from=console /dist console/dist
# Тулчейн образа вместо channel из rust-toolchain.toml: сборка воспроизводима и без загрузки rustup.
# Имя — полное, из `rustup default` (1.97.1-x86_64-…): короткое `1.97` rustup не сопоставляет с установленным
# 1.97.1 и идёт качать канал.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    RUSTUP_TOOLCHAIN="$(rustup default | cut -d' ' -f1)" cargo build --release --locked -p thepost \
      --features gssapi $([ "$CONSOLE" = 1 ] && echo --features console) \
 && install -D -m 0755 target/release/thepost /out/bin/thepost \
 && install -d -m 0750 /out/state /out/etc \
 && install -d -m 0700 /out/state/blob

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/bin/thepost /usr/local/bin/thepost
COPY --from=build /out/krb5/ /
# Каталоги создаются в сборке: в distroless нет оболочки, nonroot = 65532. Каталог BLOB (blob.url эталона smb)
# нужен до наложения Landlock — пустой том копирует его из образа (до CMP-02 узел без него не запускается).
COPY --from=build --chown=65532:65532 /out/state /var/lib/thepost
COPY --from=build --chown=65532:65532 /out/etc /etc/thepost
USER 65532:65532
WORKDIR /var/lib/thepost
VOLUME ["/var/lib/thepost"]
# SMTP, submission (465/587), IMAP (143/993), ManageSieve (4190), HTTP(S): JMAP, DAV, Admin API.
# Порты <1024 доступны nonroot: Docker задаёт net.ipv4.ip_unprivileged_port_start=0.
EXPOSE 25 465 587 143 993 4190 80 443
ENTRYPOINT ["/usr/local/bin/thepost"]
CMD ["serve", "--config", "/etc/thepost/config.yaml"]
