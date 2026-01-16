FROM ghcr.io/koa/rust-cross-compile:0.0.4 as build
COPY src /build/src
COPY Cargo.toml /build
COPY Cargo.lock /build
WORKDIR /build
ENV RUSTFLAGS -Dwarnings
#RUN cargo test
#RUN cargo clippy
RUN cargo build -r --locked --target x86_64-unknown-linux-musl

FROM scratch as run
COPY --from=build /etc/ssl /etc/ssl
COPY --from=build /build/target/x86_64-unknown-linux-musl/release/mikrotik-metric-exporter /mikrotik-metric-exporter
EXPOSE 1514/udp
ENTRYPOINT ["/mikrotik-metric-exporter"]