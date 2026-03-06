ARG JDK_VERSION=25

FROM rust:1.83-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY ym-agent/ ym-agent/
COPY ym-ecj-service/ ym-ecj-service/
RUN cargo build --release

FROM eclipse-temurin:${JDK_VERSION}-jdk-noble
COPY --from=builder /build/target/release/ym /usr/local/bin/ym
RUN ln -s /usr/local/bin/ym /usr/local/bin/ymc
WORKDIR /workspace
