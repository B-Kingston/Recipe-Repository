#!/usr/bin/env python3
"""Compare production Tesseract OCR with batched PP-OCRv6 on one frame set.

The two engines receive the same screenshots selected by src/media.rs:
frequent ffmpeg samples, uniformly thinned to at most 160 frames. PaddleOCR is
run once for the complete set, while Tesseract uses the same four-worker
per-frame baseline as production.

Example (inside the media Docker image):
    /opt/media-venv/bin/python bench/ocr/benchmark.py \
        --frames-dir bench/ocr/data/frames \
        --expected-file bench/ocr/reference-ig-post.txt \
        --output bench/ocr/out/result.json
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

OCR_MAX_FRAME_JOBS = 160
OCR_MAX_WORKERS = 4


def selected_frames(frames_dir: Path, maximum: int) -> list[Path]:
    frames = sorted(
        path
        for path in frames_dir.iterdir()
        if path.is_file() and path.suffix == ".jpg"
    )
    if not frames:
        raise RuntimeError(f"no .jpg frames found in {frames_dir}")
    stride = max(1, math.ceil(len(frames) / maximum))
    return frames[::stride]


def run_checked(
    args: list[str],
    env: dict[str, str] | None = None,
    input_bytes: bytes | None = None,
) -> bytes:
    completed = subprocess.run(
        args,
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        check=False,
    )
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(
            f"{args[0]} exited with {completed.returncode}"
            + (f": {detail[-800:]}" if detail else "")
        )
    return completed.stdout


def looks_like_token(value: str) -> bool:
    alphanumeric = sum(character.isalnum() for character in value)
    if alphanumeric < 2 or alphanumeric * 2 < len(value):
        return False
    latin = any(character.isascii() and character.isalpha() for character in value)
    arabic = any("\u0600" <= character <= "\u06ff" for character in value)
    return not (latin and arabic)


def parse_tesseract_tsv(raw: bytes) -> str:
    lines: list[tuple[tuple[str, str, str], list[str]]] = []
    for row in raw.decode("utf-8", errors="replace").splitlines()[1:]:
        fields = row.split("\t")
        if len(fields) < 12 or fields[0] != "5":
            continue
        try:
            confidence = float(fields[10])
        except ValueError:
            confidence = 0.0
        word = fields[11].strip()
        if confidence < 60.0 or not looks_like_token(word):
            continue
        key = (fields[2], fields[3], fields[4])
        if lines and lines[-1][0] == key:
            lines[-1][1].append(word)
        else:
            lines.append((key, [word]))
    output: list[str] = []
    for _, words in lines:
        if len(words) < 2 and sum(character.isalnum() for character in words[0]) < 5:
            continue
        output.extend(words)
    return " ".join(output)


def tesseract_one(path: Path) -> str:
    raw = run_checked(
        [
            os.environ.get("MEDIA_TESSERACT_PATH", "tesseract"),
            str(path),
            "stdout",
            "--psm",
            "6",
            "-l",
            os.environ.get("MEDIA_TESSERACT_LANG", "eng"),
            "tsv",
        ]
    )
    return " ".join(parse_tesseract_tsv(raw).split())


def run_tesseract(frames: list[Path]) -> tuple[float, list[str]]:
    started = time.perf_counter()
    workers = max(1, min(OCR_MAX_WORKERS, os.cpu_count() or 1, len(frames)))
    with ThreadPoolExecutor(max_workers=workers) as pool:
        readings = list(pool.map(tesseract_one, frames))
    return (time.perf_counter() - started) * 1000.0, readings


def run_paddle(
    frames: list[Path], model_size: str, batch_size: int
) -> tuple[float, dict[str, object]]:
    python = os.environ.get("MEDIA_PYTHON", sys.executable)
    script = Path(__file__).resolve().parents[2] / "pi" / "local-media.py"
    command = [
        python,
        str(script),
        "ocr",
        "--ocr-version",
        os.environ.get("MEDIA_OCR_VERSION", "PP-OCRv6"),
        "--model-size",
        model_size,
        "--lang",
        os.environ.get("MEDIA_OCR_LANG", "en"),
        "--batch-size",
        str(batch_size),
        *[str(path) for path in frames],
    ]
    started = time.perf_counter()
    raw = run_checked(command, env=os.environ.copy())
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    try:
        result = json.loads(raw)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"PaddleOCR returned invalid JSON: {error}") from error
    return elapsed_ms, result


def word_set(text: str) -> set[str]:
    return set(re.findall(r"[\w]+", text.casefold(), flags=re.UNICODE))


def quality(reference: str, readings: list[str]) -> dict[str, object]:
    expected = word_set(reference)
    observed = word_set(" ".join(readings))
    shared = expected & observed
    return {
        "reference_words": len(expected),
        "observed_unique_words": len(observed),
        "matched_reference_words": len(shared),
        "recall": round(len(shared) / len(expected), 4) if expected else None,
        "precision_against_reference": round(len(shared) / len(observed), 4)
        if observed
        else None,
    }


def engine_summary(
    name: str,
    elapsed_ms: float,
    readings: list[str],
    reference: str,
    production_snippets: list[str],
    extra: dict[str, object] | None = None,
) -> dict[str, object]:
    result: dict[str, object] = {
        "engine": name,
        "frames": len(readings),
        "nonempty_frames": sum(bool(text.strip()) for text in readings),
        "wall_ms": round(elapsed_ms, 2),
        "frames_per_second": round(len(readings) / (elapsed_ms / 1000), 3)
        if elapsed_ms > 0
        else None,
        "quality": quality(reference, readings),
        "readings": readings,
        "production": {
            "snippet_count": len(production_snippets),
            "quality": quality(reference, production_snippets),
            "snippets": production_snippets,
            "invalid_symbol_tokens": [
                token
                for text in production_snippets
                for token in text.split()
                if not all(
                    character.isalnum()
                    or character.isspace()
                    or character in "'-/°"
                    for character in token
                )
            ],
            "digit_soup_tokens": [
                token
                for text in production_snippets
                for token in text.split()
                if sum(character.isdigit() for character in token) >= 4
                or (
                    any(character.isdigit() for character in token)
                    and any(character.isalpha() for character in token)
                    and not token[0].isdigit()
                )
            ],
        },
    }
    if extra:
        result.update(extra)
    return result


def clean_with_production(binary: str, envelope: dict[str, object]) -> list[str]:
    raw = run_checked(
        [binary, "--clean-ocr"],
        env=os.environ.copy(),
        input_bytes=(json.dumps(envelope, ensure_ascii=False) + "\n").encode("utf-8"),
    )
    try:
        snippets = json.loads(raw)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"production OCR cleaner returned invalid JSON: {error}") from error
    if not isinstance(snippets, list):
        raise RuntimeError("production OCR cleaner returned a non-array")
    return [
        str(snippet.get("text", ""))
        for snippet in snippets
        if isinstance(snippet, dict) and str(snippet.get("text", "")).strip()
    ]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--frames-dir", type=Path, required=True)
    parser.add_argument("--expected-file", type=Path)
    parser.add_argument("--max-frames", type=int, default=OCR_MAX_FRAME_JOBS)
    parser.add_argument(
        "--model-size", choices=["medium", "small", "tiny"], default="small"
    )
    parser.add_argument("--batch-size", type=int, default=8)
    parser.add_argument(
        "--app-path",
        default=os.environ.get("MEDIA_APP_PATH", "kindle-recipes"),
        help="production binary exposing --clean-ocr",
    )
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.max_frames < 1 or args.batch_size < 1:
        raise RuntimeError("--max-frames and --batch-size must be positive")

    frames = selected_frames(args.frames_dir, min(args.max_frames, OCR_MAX_FRAME_JOBS))
    reference = args.expected_file.read_text(encoding="utf-8") if args.expected_file else ""
    paddle_ms, paddle_result = run_paddle(
        frames, args.model_size, args.batch_size
    )
    paddle_readings = [
        str(frame.get("text", ""))
        for frame in paddle_result.get("frames", [])
        if isinstance(frame, dict)
    ]
    if len(paddle_readings) != len(frames):
        raise RuntimeError(
            f"PaddleOCR returned {len(paddle_readings)} readings for {len(frames)} frames"
        )
    tesseract_ms, tesseract_readings = run_tesseract(frames)
    paddle_snippets = clean_with_production(args.app_path, paddle_result)
    tesseract_envelope = {
        "frames": [
            {"text": text, "seconds": index * 0.5}
            for index, text in enumerate(tesseract_readings)
        ]
    }
    tesseract_snippets = clean_with_production(args.app_path, tesseract_envelope)
    report = {
        "frames_dir": str(args.frames_dir),
        "frame_count": len(frames),
        "model_size": args.model_size,
        "batch_size": args.batch_size,
        "reference_file": str(args.expected_file) if args.expected_file else None,
        "production_cleaner": args.app_path,
        "paddleocr": engine_summary(
            "PP-OCRv6",
            paddle_ms,
            paddle_readings,
            reference,
            paddle_snippets,
            {
                "model": paddle_result.get("model", "PP-OCRv6"),
                "lang": paddle_result.get("lang", os.environ.get("MEDIA_OCR_LANG", "en")),
            },
        ),
        "tesseract": engine_summary(
            "Tesseract",
            tesseract_ms,
            tesseract_readings,
            reference,
            tesseract_snippets,
        ),
    }
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        raise SystemExit(1)
