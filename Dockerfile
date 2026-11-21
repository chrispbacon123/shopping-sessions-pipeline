# Shopping Sessions: static Rust binary on an empty (scratch) image.
# Scorer: linux/amd64 Intel Xeon, 8 vCPU, runs as uid 1000, read-only root, no network.
FROM rust:1.85-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
ENV CARGO_NET_RETRY=5 \
    RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static"
RUN cargo build --release --locked --target x86_64-unknown-linux-gnu \
 && strip target/x86_64-unknown-linux-gnu/release/shopping_sessions \
 && cp target/x86_64-unknown-linux-gnu/release/shopping_sessions /shopping_sessions

FROM scratch
COPY --from=build /shopping_sessions /shopping_sessions
USER 1000:1000
ENTRYPOINT ["/shopping_sessions"]
