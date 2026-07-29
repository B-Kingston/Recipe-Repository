FROM rust:1.88-slim AS builder
WORKDIR /app
COPY Cargo.toml ./
RUN mkdir src && printf 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY . .
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 app && mkdir /data && chown app:app /data
WORKDIR /app
COPY --from=builder /app/target/release/kindle-recipes /usr/local/bin/kindle-recipes
COPY --from=builder /app/templates /app/templates
COPY --from=builder /app/static /app/static
USER app
ENV DATABASE_URL=sqlite:/data/recipes.sqlite3 APP_BIND=0.0.0.0:3000
EXPOSE 3000
VOLUME ["/data"]
CMD ["kindle-recipes"]
