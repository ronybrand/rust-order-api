# One image per process: `docker build --build-arg TARGET=api .` or `TARGET=worker`.
#
# The result is a single statically linked binary on an empty filesystem (`scratch`). There is no shell, no package
# manager and no libc in it, which is the point: there is nothing to patch and nothing to exploit besides the binary.
# TLS roots are compiled in (webpki-roots), so no CA bundle is needed either.
# The tag rides along with the digest: it names the image the digest belongs to (Rust 1.99.1 on Alpine) and the
# variant the apk call below requires, which a bare digest hides from anyone reading the file.
FROM rust:1.99.1-alpine@sha256:3ea887915f261f273d6a24efdfdb92665dee12199923e57870453644fe4ae5c9 AS build
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
