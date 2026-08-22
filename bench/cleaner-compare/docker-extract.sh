#!/bin/sh
# Extraction wrapper for bench/cleaner-compare/compare.mjs (BENCH_APP_BIN).
#
# The host machine deliberately carries no media toolchain (yt-dlp, ffmpeg,
# tesseract, faster-whisper); the production Docker image is the only place
# they exist. This script forwards the extractor invocation into a throwaway
# container and streams the JSON evidence back on stdout. The recipe-data
# volume is mounted so the Whisper model download persists between runs.
set -eu
cd "$(dirname "$0")/../.."
exec docker run --rm --entrypoint /usr/local/bin/kindle-recipes \
    -v recipe-data:/data kindle-recipes:bench "$@"
