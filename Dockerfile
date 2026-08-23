FROM node:22-bookworm-slim AS pi-deps
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev

FROM rust:1.88-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    mkdir src && printf 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    touch src/main.rs && cargo build --release

FROM node:22-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    python3 \
    python3-venv \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/media-venv \
    && /opt/media-venv/bin/pip install --no-cache-dir paddlepaddle==3.2.0 \
        -i https://www.paddlepaddle.org.cn/packages/stable/cpu/ \
    && /opt/media-venv/bin/pip install --no-cache-dir paddleocr==3.7.0 faster-whisper==1.2.1 yt-dlp==2026.8.19 \
    && useradd --system --uid 10001 app \
    && mkdir -p /data/media-models \
    && chown -R app:app /data /opt/media-venv
WORKDIR /app
COPY --from=builder /app/target/release/kindle-recipes /usr/local/bin/kindle-recipes
COPY --from=builder /app/templates /app/templates
COPY --from=builder /app/static /app/static
COPY --from=builder /app/pi /app/pi
COPY --from=builder /app/package.json /app/package.json
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
