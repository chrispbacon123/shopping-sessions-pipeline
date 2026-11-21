# Shopping Sessions: static musl binary on scratch (MIT)
FROM rust:1.85-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
ENV RUSTFLAGS="-C target-cpu=x86-64-v3 -C link-self-contained=yes" CARGO_NET_RETRY=5
RUN cargo build --release --locked && strip target/release/shopping_sessions

FROM scratch
COPY --from=build /src/target/release/shopping_sessions /shopping_sessions
USER 1000:1000
ENTRYPOINT ["/shopping_sessions"]
