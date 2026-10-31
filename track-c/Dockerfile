FROM rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS build
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
# x86-64-v3 provides the AVX2 that records.rs uses; a static binary starts
# without the dynamic loader.
ENV RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static,+prfchw"
RUN cargo build --release

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
COPY --from=build /src/target/release/track-c /track-c
ENTRYPOINT ["/track-c"]
