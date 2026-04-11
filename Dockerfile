# Stage 1: Build
FROM rust:1.75-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/
COPY src/ src/
COPY config/ config/
RUN cargo generate-lockfile 2>/dev/null || true && cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -r -s /bin/false sre
COPY --from=builder /app/target/release/sre-agents /usr/local/bin/
COPY --from=builder /app/config/default.toml /app/config/default.toml
USER sre
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/sre-agents", "--help"]
ENTRYPOINT ["sre-agents"]
CMD ["serve", "--config", "/app/config/default.toml"]
