FROM rust:1.89-bookworm@sha256:948f9b08a66e7fe01b03a98ef1c7568292e07ec2e4fe90d88c07bb14563c84ff AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY design/v3/fonts ./design/v3/fonts
RUN cargo build --release --locked --features central-prototype --bin codexctl-central

FROM node:22-trixie-slim@sha256:b26b04c123d9ff8ab646ceb18b9d75a1173acf64b9a401094b906d27b29338d4
ARG CODEX_VERSION=0.159.0
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && npm install --global @openai/codex@${CODEX_VERSION} \
    && npm cache clean --force \
    && groupadd --gid 10001 codexctl \
    && useradd --uid 10001 --gid 10001 --create-home codexctl
COPY --from=build /source/target/release/codexctl-central /usr/local/bin/codexctl-central
USER 10001:10001
ENV HOME=/home/codexctl
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 CMD ["node", "-e", "fetch('http://127.0.0.1:8787/health').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"]
ENTRYPOINT ["codexctl-central"]
