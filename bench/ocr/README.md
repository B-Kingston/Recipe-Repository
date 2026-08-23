# OCR benchmark

`benchmark.py` measures the production PP-OCRv6-small pipeline on JPEG
screenshots. It uses the same 160-frame cap as `src/media.rs`. When a larger
uniform frame set is supplied, it selects exactly 160 evenly spaced frames
rather than losing almost half the set to a ceil stride. Both raw engine
readings and the final production `clean_ocr_reading`/`collapse_ocr_readings`
output are reported; the latter is invoked through the app's `--clean-ocr`
command so this script does not carry a second cleanup implementation.

```sh
docker run --rm --user root \
  -v "$PWD":/workspace -v recipe-data:/data -w /workspace \
  -e MEDIA_OCR_SCORE_THRESHOLD=0.70 \
  kindle-recipes:paddle-ocr \
  /opt/media-venv/bin/python bench/ocr/benchmark.py \
  --frames-dir /workspace/bench/ocr/data/<reel>/frames \
  --frame-step-seconds 0.25 \
  --output /workspace/bench/ocr/data/<reel>/results/small.json
```

Set `--frame-step-seconds` to the extraction grid before any benchmark
thinning (0.25 for the nominal production 4 Hz default). For an adaptive plan,
pass `--timestamps-file path/to/timestamps.json` containing one source second
per JPEG; it takes precedence over the fixed step. The selected source indexes
and seconds are included in the report and passed into the production cleaner,
so a capped or adaptive set does not acquire fake consecutive timestamps.

`--expected-file` is optional. The fresh URL runs in
`bench/benchmark-report.md` intentionally omit recall/precision because no
human reference transcript was supplied; they report frame coverage, final
snippets, and safety checks instead. The default Paddle line-confidence gate
is 0.70; override it with `MEDIA_OCR_SCORE_THRESHOLD` when testing another
calibration. The `production.invalid_symbol_tokens` and
`production.digit_soup_tokens` lists are hard checks for random symbols and
counter-like OCR failures.
