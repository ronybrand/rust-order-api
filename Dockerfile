# One image per process: `docker build --build-arg TARGET=api .` or `TARGET=worker`.
#
# The result is a single statically linked binary on an empty filesystem (`scratch`). There is no shell, no package
# manager and no libc in it, which is the point: there is nothing to patch and nothing to exploit besides the binary.
# TLS roots are compiled in (webpki-roots), so no CA bundle is needed either.
FROM rust:1.99-alpine@sha256:557a94c82b485a14b569120e00867267108c734efe33f10cc31ca0cd5dd1eca9 AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src
# --locked: the build uses exactly the Cargo.lock that was reviewed and scanned.
RUN cargo build --release --locked --bins

FROM scratch
ARG TARGET=api
COPY --from=build /src/target/release/${TARGET} /app
# A numeric user: there is no /etc/passwd to resolve a name in.
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/app"]
