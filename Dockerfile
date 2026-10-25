# Node is pinned to a patch release: 22.23.4 is the first 22.x with the
# bundled Brotli decoder fixed (CVE-2026-111497), and a floating tag could
# build from an older cached image. Raise it, never lower it.
FROM node:22.23.4-bookworm-slim AS pi-deps
WORKDIR /app
COPY package.json package-lock.json ./
RUN --mount=type=cache,id=kindle-recipes-npm,target=/root/.npm,sharing=locked \
    npm ci --omit=dev --prefer-offline --cache=/root/.npm

# The browser scripts: web/*.ts compiled to one ES5 static/app.js, and htmx
# lowered to ES5 as static/htmx.js (older Kindle browsers parse nothing newer).
FROM node:22.23.4-bookworm-slim AS web
WORKDIR /app
COPY package.json package-lock.json ./
RUN --mount=type=cache,id=kindle-recipes-npm,target=/root/.npm,sharing=locked \
    npm ci --prefer-offline --cache=/root/.npm
COPY web ./web
RUN npm run build:web

FROM rust:1.88-slim AS builder
WORKDIR /app
RUN --mount=type=cache,id=kindle-recipes-builder-apt-archives,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=kindle-recipes-builder-apt-lists,target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends ffmpeg
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

FROM node:22.23.4-bookworm-slim
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
COPY --from=web /app/static/app.js /app/static/htmx.js /app/static/
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
