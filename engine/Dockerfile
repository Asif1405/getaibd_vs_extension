FROM rust:1.85-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs
RUN cargo build --release 2>/dev/null || true
RUN rm -rf src

COPY src/ src/
RUN touch src/main.rs src/lib.rs
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN useradd -r -s /bin/false mcp
USER mcp

COPY --from=builder /app/target/release/mcp-universal /usr/local/bin/mcp-universal

EXPOSE 3333

ENV MCP_SERVER_HOST=0.0.0.0

ENTRYPOINT ["mcp-universal"]
