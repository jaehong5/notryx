FROM rust:1.99.1-slim-bookworm@sha256:86234e857a413004348b471d11a4ebe1aa92d2eae2b6b143cac68fd1ac3c48be AS build
WORKDIR /workspace
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo/config.toml .cargo/config.toml
COPY crates/ crates/
COPY apps/server/ apps/server/
# Retain workspace resolution without compiling Tauri or installing Android targets.
COPY apps/client/src-tauri/ apps/client/src-tauri/
RUN cargo +1.99.1 build --locked --release -p notryx-server --bin notryx-server -j 2

FROM build AS verify
RUN cargo +1.99.1 test --locked -p notryx-server -p notryx-server-storage --features notryx-server-storage/test-hooks -j 2

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8 AS runtime
WORKDIR /app
COPY --from=build --chmod=0555 /workspace/target/release/notryx-server /app/notryx-server
USER 1000:1000
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=120s --retries=3 \
    CMD ["/app/notryx-server", "healthcheck", "--address", "127.0.0.1:8080"]
ENTRYPOINT ["/app/notryx-server"]
CMD ["serve", "--config", "/etc/notryx/server.toml"]
