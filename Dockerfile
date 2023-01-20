## Build stage
FROM rust as builder
RUN rustup toolchain add nightly
RUN rustup default nightly
RUN cargo +nightly install afl

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y cmake clang

## Add source code
ADD . /roxmltree
WORKDIR /roxmltree/testing-tools/afl-fuzz

RUN cargo afl build

# Package Stage
FROM ubuntu:20.04
COPY --from=builder /roxmltree/testing-tools/afl-fuzz/target/debug/afl-fuzz /
