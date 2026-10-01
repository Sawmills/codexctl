FROM rust:1.89-bookworm AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --features central-prototype --bin codexctl-central

FROM node:22-bookworm-slim
ARG CODEX_VERSION=0.159.0
RUN npm install --global @openai/codex@${CODEX_VERSION} \
    && npm cache clean --force \
    && groupadd --gid 10001 codexctl \
    && useradd --uid 10001 --gid 10001 --create-home codexctl
COPY --from=build /source/target/release/codexctl-central /usr/local/bin/codexctl-central
USER 10001:10001
ENV HOME=/home/codexctl
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 CMD ["node", "-e", "fetch('http://127.0.0.1:8787/health').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"]
ENTRYPOINT ["codexctl-central"]
