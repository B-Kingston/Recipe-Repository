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
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 app && mkdir /data && chown app:app /data
WORKDIR /app
COPY --from=builder /app/target/release/kindle-recipes /usr/local/bin/kindle-recipes
COPY --from=builder /app/templates /app/templates
COPY --from=builder /app/static /app/static
COPY --from=builder /app/pi /app/pi
COPY --from=builder /app/package.json /app/package.json
COPY --from=pi-deps /app/node_modules /app/node_modules
USER app
ENV DATABASE_URL=sqlite:/data/recipes.sqlite3 APP_BIND=0.0.0.0:3000 PI_CODING_AGENT_DIR=/data/pi-agent
EXPOSE 3000
VOLUME ["/data"]
CMD ["kindle-recipes"]
