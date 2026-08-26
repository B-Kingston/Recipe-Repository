FROM node:22-bookworm-slim AS pi-deps
WORKDIR /app
COPY package.json package-lock.json ./
RUN --mount=type=cache,id=kindle-recipes-npm,target=/root/.npm,sharing=locked \
    npm ci --omit=dev --prefer-offline --cache=/root/.npm

FROM rust:1.88-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,id=kindle-recipes-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=kindle-recipes-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=kindle-recipes-cargo-target,target=/app/target,sharing=locked \
    mkdir src && printf 'fn main() {}' > src/main.rs && \
    cargo build --release --locked && rm -rf src
COPY src ./src
COPY templates ./templates
COPY migrations ./migrations
RUN --mount=type=cache,id=kindle-recipes-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=kindle-recipes-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=kindle-recipes-cargo-target,target=/app/target,sharing=locked \
    touch src/main.rs && cargo build --release --locked && \
    cp target/release/kindle-recipes /app/kindle-recipes

FROM node:22-bookworm-slim
RUN --mount=type=cache,id=kindle-recipes-apt-archives,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=kindle-recipes-apt-lists,target=/var/lib/apt/lists,sharing=locked \
    --mount=type=cache,id=kindle-recipes-pip,target=/root/.cache/pip,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    python3 \
    python3-venv \
    && python3 -m venv /opt/media-venv \
    && /opt/media-venv/bin/pip install --disable-pip-version-check \
        --cache-dir=/root/.cache/pip \
        paddlepaddle==3.2.0 \
        -i https://www.paddlepaddle.org.cn/packages/stable/cpu/ \
    && /opt/media-venv/bin/pip install --disable-pip-version-check \
        --cache-dir=/root/.cache/pip \
        paddleocr==3.7.0 faster-whisper==1.2.1 yt-dlp==2026.8.19 \
    && useradd --system --uid 10001 app \
    && mkdir -p /data/media-models \
    && chown -R app:app /data /opt/media-venv
WORKDIR /app
COPY --from=builder /app/kindle-recipes /usr/local/bin/kindle-recipes
COPY static /app/static
COPY templates /app/templates
COPY pi /app/pi
COPY --from=pi-deps /app/package.json /app/package.json
COPY --from=pi-deps /app/node_modules /app/node_modules
USER app
ENV PATH=/opt/media-venv/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    DATABASE_URL=sqlite:/data/recipes.sqlite3 \
    APP_BIND=0.0.0.0:3000 \
    PI_CODING_AGENT_DIR=/data/pi-agent \
    MEDIA_PYTHON=/opt/media-venv/bin/python \
    MEDIA_MODEL_CACHE=/data/media-models \
    PADDLE_PDX_CACHE_HOME=/data/media-models \
    HF_HOME=/data/media-models \
    XDG_CACHE_HOME=/data/media-models
EXPOSE 3000
VOLUME ["/data"]
CMD ["kindle-recipes"]
