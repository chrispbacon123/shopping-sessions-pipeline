# Shopping Sessions: native Rust pipeline (MIT). Builds a single static-ish binary.
FROM rust:1.85-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
ENV RUSTFLAGS="-C target-cpu=x86-64-v3" CARGO_NET_RETRY=5
RUN cargo build --release --locked && strip target/release/shopping_sessions

FROM debian:bookworm-slim
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app
COPY --from=build /src/target/release/shopping_sessions /app/shopping_sessions
USER 1000
CMD ["/app/shopping_sessions"]
