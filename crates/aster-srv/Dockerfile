# aster-srv image. Build from the repository root:
#   docker build -f crates/aster-srv/Dockerfile -t aster-srv .
FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p aster-srv

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/aster-srv /usr/local/bin/aster-srv
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/aster-srv"]
