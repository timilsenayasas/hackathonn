FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src
RUN cargo build --release --bin fastsess && \
    cp target/x86_64-unknown-linux-gnu/release/fastsess /fastsess && \
    strip /fastsess

FROM scratch
COPY --from=build /fastsess /fastsess
USER 1000:1000
ENTRYPOINT ["/fastsess"]
