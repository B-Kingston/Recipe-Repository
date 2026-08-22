# Fresh OCR benchmark report

This report replaces the previous generated benchmark report. Before these
runs, the old downloaded videos, frame sets, OCR result files, output
folders, and the old `reference-ig-post.txt` artifact were removed. The
benchmark harness and small regression fixtures used by Rust unit tests were
retained.

## Run configuration

- Docker image: `kindle-recipes:paddle-ocr` rebuilt from the current tree
- PaddleOCR: PP-OCRv6 `medium`, `small`, and `tiny`
- Default/production model: `small`
- Paddle line-confidence threshold: `0.70`
- Paddle language: `en`
- Tesseract baseline: `eng`, `--psm 6`, four workers
- Frames: 2 Hz, grayscale, 768px preprocessing, batch size 8, maximum 160
- Final snippets: exact production Rust `clean_ocr_reading` plus temporal
  collapse, invoked with `kindle-recipes --clean-ocr`
- No human reference transcript was supplied for the new reel, so
  recall/precision are intentionally not reported. The run records frame
  coverage and final-output safety checks instead.

Result JSON and evidence for each URL are under
`bench/ocr/data/<reel-id>/results/` and `bench/ocr/data/<reel-id>/evidence.json`.

## Reel 1: `DbJNNc8IBxh`

URL: <https://www.instagram.com/reel/DbJNNc8IBxh/>

- Uploader: James Teng Sun Ooi (`jimmy_chews`)
- Video: 78.272 seconds, 720x1280, 30 fps
- Benchmark frames: 156
- Production adaptive extraction: 7 final OCR snippets, 1,208 audio
  transcript characters, no warnings

| Engine | OCR time | Non-empty frames | Final snippets | Invalid symbols | Digit soup |
|---|---:|---:|---:|---:|---:|
| PP-OCRv6 medium | 310.81s | 83/156 | 18 | 0 | 0 |
| PP-OCRv6 small | 75.56s | 75/156 | 11 | 0 | 0 |
| PP-OCRv6 tiny | 36.90s | 67/156 | 7 | 0 | 0 |
| Tesseract `eng` baseline (medium run) | 9.85s | 33/156 | 1 | 0 | 0 |

Production adaptive OCR output:

```text
0s   ALL-PURPOSE
8s   SUGAR
14s  MASTER SAUCE
39s  MASTER SAUCE
61s  GOCHUJANG
66s  BASIC
72s  ASIAN
```

The post caption describes a master stir-fry sauce. The local audio contains
more complete recipe facts than the on-screen text, including 6 parts light
soy sauce, 2 parts oyster sauce, 1 part shallot, 1 part dark soy sauce, 1
part sugar, 0.5 white pepper, timing, sesame oil, MSG, and optional gochujang.

The raw engines produced some rejected noise (`200M1`, long counters, and
misreads such as `SHUCE`); none reached the production output.

## Reel 2: `DZNQT3Pt3Ja`

URL: <https://www.instagram.com/p/DZNQT3Pt3Ja/>

- Uploader: Mohammed Moosa (`chefjjskitchen`)
- Video: 42.676 seconds, 720x1280, 30 fps
- Benchmark frames: 85
- Production adaptive extraction: 26 final OCR snippets, 641 audio
  transcript characters, no warnings

| Engine | OCR time | Non-empty frames | Final snippets | Invalid symbols | Digit soup |
|---|---:|---:|---:|---:|---:|
| PP-OCRv6 medium | 180.50s | 83/85 | 24 | 0 | 0 |
| PP-OCRv6 small | 42.36s | 82/85 | 24 | 0 | 0 |
| PP-OCRv6 tiny | 20.75s | 82/85 | 24 | 0 | 0 |
| Tesseract `eng` baseline (medium run) | 13.52s | 42/85 | 11 | 0 | 0 |

Production adaptive OCR output:

```text
0s   Rice skewers
1s   Place 6 tomatoes in a lightly oiled pan
3s   With a diced onion in the middle
5s   A head of garlic
6s   A bit of chili
7s   let them cook on low for 10 minutes
9s   Till the tomatoes are soft
10s  you can easily remove the
11s  Mash everything very well
13s  Till you get the most amazingly aromatic mix
16s  Season with a hefty pinch of
17s  A tsp of black pepper
18s  Chicken stock powder
19s  a tbsp of smoked paprika
21s  Once it takes that deep red color
23s  Add 2 cups of long grain cooked white rice
25s  That doesn't easily break
27s  Just like Canary
28s  Fold the rice with the mix
29s  Place it on the serving plate
31s  Top it with a massive bunch of spring onions
33s  And some leftover BBQ skewers
34s  I got addicted to this recipe as it's easy
37s  uses all the leftovers in the fridge
39s  Serve it with any sauce you
41s  Thank me later
```

This reel retained the strongest OCR quality: the production output contains
quantities (`6`, `10`, `tsp`, `tbsp`, and `2 cups`), repaired word spacing, and
no random symbols or digit-soup tokens. A few displayed cards are naturally
sentence fragments because the text itself continues on the next card; no
individual word is broken.

## Verification

- Docker Rust formatting check: passed
- Docker Rust tests: **71 passed, 0 failed**
- Docker image build: passed
- Both URLs downloaded successfully with yt-dlp
- Production extraction completed for both URLs with zero warnings
- Every final Paddle and Tesseract result reported `invalid_symbol_tokens=0`
  and `digit_soup_tokens=0`
