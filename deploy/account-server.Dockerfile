# Claude-only image: no Codex binary, Node runtime, or OpenAI account required.
FROM rust:1.89-bookworm@sha256:948f9b08a66e7fe01b03a98ef1c7568292e07ec2e4fe90d88c07bb14563c84ff AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --features central-prototype --bin account-server
RUN set -eu; \
    mkdir -p /runtime/etc/ssl /runtime/tmp; \
    chmod 1777 /runtime/tmp; \
    cp -a /etc/ssl/certs /runtime/etc/ssl/; \
    ldd /source/target/release/account-server | awk '/=> \// {print $3} /^[[:space:]]*\// {print $1}' \
      | xargs -r -I '{}' cp -L --parents '{}' /runtime

# Reuse the build image's matching glibc and TLS roots without adding a package manager.
FROM scratch
COPY --from=build /runtime /
COPY --from=build /source/target/release/account-server /account-server
USER 10001:10001
ENV HOME=/state
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 CMD ["/account-server", "health-check"]
ENTRYPOINT ["/account-server"]
