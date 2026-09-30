FROM rust:1-bookworm AS build
WORKDIR /src
# Build dependencies on their own layer so code changes don't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY src src
COPY migrations migrations
COPY web web
RUN touch src/main.rs && cargo build --release --locked

FROM python:3.13-slim-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir "yt-dlp[default]" gallery-dl
# yt-dlp needs a JavaScript runtime for YouTube.
COPY --from=denoland/deno:bin /deno /usr/local/bin/deno
COPY --from=build /src/target/release/mealie-forager /usr/local/bin/mealie-forager

RUN useradd --system --uid 1000 --home-dir /data forager \
    && mkdir -p /data \
    && chown forager /data
USER forager
VOLUME /data
ENV LISTEN_ADDR=0.0.0.0:3000 \
    DATABASE_PATH=/data/mealie-forager.db \
    WORK_DIR=/tmp/mealie-forager \
    XDG_CACHE_HOME=/tmp/mealie-forager/cache
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s \
    CMD python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:3000/healthz')"
ENTRYPOINT ["mealie-forager"]
