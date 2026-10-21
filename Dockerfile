FROM rust:1.85-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p jev-provider

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 app
COPY --from=build /app/target/release/jev-provider /usr/local/bin/jev-provider
USER 10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/jev-provider"]
