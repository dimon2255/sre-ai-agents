# Stage 1: Build
FROM rust:1.97-bookworm AS builder
WORKDIR /app
# protoc is required by build.rs to compile the gRPC service definitions (Phase 2b).
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ proto/
COPY crates/ crates/
COPY src/ src/
COPY config/ config/
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -r -s /bin/false sre
RUN mkdir -p /app/data && chown sre:sre /app/data
COPY --from=builder /app/target/release/sre-agents /usr/local/bin/
COPY --from=builder /app/config/default.toml /app/config/default.toml
USER sre
WORKDIR /app
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/sre-agents", "--help"]
ENTRYPOINT ["sre-agents"]
CMD ["--config", "/app/config/default.toml", "serve"]
