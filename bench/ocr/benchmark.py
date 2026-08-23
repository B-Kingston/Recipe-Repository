#!/usr/bin/env python3
"""Benchmark production PP-OCRv6-small on one frame set.

The screenshots are selected by src/media.rs and uniformly thinned to at most
160 frames. PaddleOCR small is run once for the complete set.

Example (inside the media Docker image):
    /opt/media-venv/bin/python bench/ocr/benchmark.py \
        --frames-dir bench/ocr/data/frames \
        --expected-file bench/ocr/reference-ig-post.txt \
        --output bench/ocr/out/result.json
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

OCR_MAX_FRAME_JOBS = 160


def selected_frames(frames_dir: Path, maximum: int) -> tuple[list[Path], list[int]]:
    frames = sorted(
        path
        for path in frames_dir.iterdir()
        if path.is_file() and path.suffix == ".jpg"
    )
    if not frames:
        raise RuntimeError(f"no .jpg frames found in {frames_dir}")
    if len(frames) <= maximum:
        return frames, list(range(len(frames)))
    if maximum == 1:
        return [frames[0]], [0]
    # Pick exactly N positions across the full timeline. A ceil stride can
    # throw away almost half the data (161 inputs became 81 at a cap of 160).
    indexes = [
        round(step * (len(frames) - 1) / (maximum - 1))
        for step in range(maximum)
    ]
    return [frames[index] for index in indexes], indexes


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


def run_paddle(
    frames: list[Path], batch_size: int
) -> tuple[float, dict[str, object]]:
    python = os.environ.get("MEDIA_PYTHON", sys.executable)
    script = Path(__file__).resolve().parents[2] / "pi" / "local-media.py"
    command = [
        python,
        str(script),
        "ocr",
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
        "--frame-step-seconds",
        type=float,
        default=0.5,
        help="seconds between source frames before benchmark thinning",
    )
    parser.add_argument(
        "--timestamps-file",
        type=Path,
        help="optional JSON array of source seconds, one per input JPEG",
    )
    parser.add_argument("--batch-size", type=int, default=8)
    parser.add_argument(
        "--app-path",
        default=os.environ.get("MEDIA_APP_PATH", "kindle-recipes"),
        help="production binary exposing --clean-ocr",
    )
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.max_frames < 1 or args.batch_size < 1 or args.frame_step_seconds <= 0:
        raise RuntimeError("frame step, --max-frames, and --batch-size must be positive")

    frames, source_indexes = selected_frames(
        args.frames_dir, min(args.max_frames, OCR_MAX_FRAME_JOBS)
    )
    if args.timestamps_file:
        all_seconds = json.loads(args.timestamps_file.read_text(encoding="utf-8"))
        if not isinstance(all_seconds, list) or not all(
            isinstance(value, (int, float)) and value >= 0 for value in all_seconds
        ):
            raise RuntimeError("--timestamps-file must contain a JSON array of nonnegative seconds")
        total_inputs = max(source_indexes) + 1 if source_indexes else 0
        if len(all_seconds) < total_inputs:
            raise RuntimeError("--timestamps-file has fewer entries than the input frame set")
        frame_seconds = [float(all_seconds[index]) for index in source_indexes]
    else:
        frame_seconds = [index * args.frame_step_seconds for index in source_indexes]
    reference = args.expected_file.read_text(encoding="utf-8") if args.expected_file else ""
    paddle_ms, paddle_result = run_paddle(frames, args.batch_size)
    paddle_readings = [
        str(frame.get("text", ""))
        for frame in paddle_result.get("frames", [])
        if isinstance(frame, dict)
    ]
    if len(paddle_readings) != len(frames):
        raise RuntimeError(
            f"PaddleOCR returned {len(paddle_readings)} readings for {len(frames)} frames"
        )
    # The engine preserves input order but does not know the source timeline.
    # Attach real pre-thinning timestamps before invoking production cleanup.
    for frame, seconds in zip(paddle_result.get("frames", []), frame_seconds):
        if isinstance(frame, dict):
            frame["seconds"] = seconds
    paddle_snippets = clean_with_production(args.app_path, paddle_result)
    report = {
        "frames_dir": str(args.frames_dir),
        "frame_count": len(frames),
        "frame_step_seconds": None if args.timestamps_file else args.frame_step_seconds,
        "timestamps_file": str(args.timestamps_file) if args.timestamps_file else None,
        "frame_seconds": frame_seconds,
        "model_size": "small",
        "batch_size": args.batch_size,
        "reference_file": str(args.expected_file) if args.expected_file else None,
        "production_cleaner": args.app_path,
        "paddleocr": engine_summary(
            "PP-OCRv6-small",
            paddle_ms,
            paddle_readings,
            reference,
            paddle_snippets,
            {
                "model": paddle_result.get("model", "PP-OCRv6_small"),
                "lang": paddle_result.get("lang", os.environ.get("MEDIA_OCR_LANG", "en")),
            },
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
