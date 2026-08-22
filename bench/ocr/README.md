# OCR benchmark

`benchmark.py` compares the production Tesseract baseline with batched
PP-OCRv6 on the same JPEG screenshots. It uses the same 160-frame cap and
four-worker Tesseract baseline as `src/media.rs`. Both raw engine readings and
the final production `clean_ocr_reading`/`collapse_ocr_readings` output are
reported; the latter is invoked through the app's `--clean-ocr` benchmark
command so this script does not carry a second cleanup implementation. Use
`--model-size medium`, `small`, or `tiny`. Supply screenshots extracted from a
real video; do not compare OCR engines on different frame sets.

```sh
docker run --rm --user root \
  -v "$PWD":/workspace -v recipe-data:/data -w /workspace \
  -e MEDIA_OCR_SCORE_THRESHOLD=0.70 \
  kindle-recipes:paddle-ocr \
  /opt/media-venv/bin/python bench/ocr/benchmark.py \
  --frames-dir /workspace/bench/ocr/data/<reel>/frames \
  --model-size small \
  --output /workspace/bench/ocr/data/<reel>/results/small.json
```

`--expected-file` is optional. The fresh URL runs in
`bench/benchmark-report.md` intentionally omit recall/precision because no
human reference transcript was supplied; they report frame coverage, final
snippets, and safety checks instead. The default Paddle line-confidence gate
is 0.70; override it with `MEDIA_OCR_SCORE_THRESHOLD` when testing another
calibration. The `production.invalid_symbol_tokens` and
`production.digit_soup_tokens` lists are hard checks for random symbols and
counter-like OCR failures.
