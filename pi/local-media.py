#!/usr/bin/env python3
"""Local CPU speech-to-text helper for the recipe video importer.

The Rust app invokes this as a short-lived process after extracting a mono
16 kHz WAV with ffmpeg. faster-whisper downloads the selected model once into
the configured local cache and performs all transcription locally; no audio is
sent to an AI API.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path


def _env_int(name: str, default: int, minimum: int = 1) -> int:
    try:
        value = int(os.environ.get(name, str(default)))
    except ValueError:
        return default
    return max(minimum, value)


def _configure_paddle_cache() -> None:
    # PaddleX reads this variable while it is imported, so configure the
    # persistent media cache before importing PaddleOCR. Keeping the models
    # beside Whisper means the Docker volume survives container rebuilds.
    cache_dir = os.environ.get("MEDIA_MODEL_CACHE", "").strip()
    if cache_dir:
        os.environ.setdefault("PADDLE_PDX_CACHE_HOME", cache_dir)
        os.environ.setdefault("PADDLE_HOME", cache_dir)


def _paddle_result_payload(result: object) -> dict[str, object]:
    """Return the JSON-safe result payload across PaddleOCR 3.x releases."""
    value = getattr(result, "json", None)
    if callable(value):
        value = value()
    if isinstance(value, str):
        value = json.loads(value)
    if not isinstance(value, dict):
        return {}
    payload = value.get("res", value)
    return payload if isinstance(payload, dict) else {}


def _box_sort_key(box: object, fallback: int) -> tuple[float, float, int]:
    try:
        points = list(box)  # type: ignore[arg-type]
        xs = [float(point[0]) for point in points]
        ys = [float(point[1]) for point in points]
        return (min(ys), min(xs), fallback)
    except (TypeError, ValueError, IndexError):
        return (float(fallback), float(fallback), fallback)


def _text_matches_language(text: str, lang: str) -> bool:
    """Reject confident-looking output in a script the selected model cannot read.

    PP-OCR can occasionally interpret food texture as repeated CJK glyphs even
    when the explicit English recognizer is loaded. Confidence alone cannot
    remove that failure because the recognizer is highly certain. Keep CJK for
    the language families that support it and reject CJK-majority lines for
    the normal English/Arabic recipe configurations.
    """
    letters = [character for character in text if character.isalpha()]
    if not letters:
        return True
    cjk = sum(
        "\u3400" <= character <= "\u4dbf"
        or "\u4e00" <= character <= "\u9fff"
        or "\uf900" <= character <= "\ufaff"
        for character in letters
    )
    language = lang.strip().lower()
    supports_cjk = language in {"ch", "zh"} or language.startswith(
        ("chinese", "japan", "korean")
    )
    return supports_cjk or cjk * 2 <= len(letters)


def _paddle_frame_reading(
    result: object, score_threshold: float, lang: str
) -> dict[str, object]:
    payload = _paddle_result_payload(result)
    texts = payload.get("rec_texts") or []
    scores = payload.get("rec_scores") or []
    boxes = payload.get("rec_boxes") or payload.get("rec_polys") or []
    rows: list[tuple[tuple[float, float, int], str, float]] = []
    for index, raw_text in enumerate(texts):
        text = " ".join(str(raw_text).split())
        if not text:
            continue
        try:
            score = float(scores[index])
        except (IndexError, TypeError, ValueError):
            score = 0.0
        if score < score_threshold or not _text_matches_language(text, lang):
            continue
        # A lone short token is usually background texture, while a quantity
        # remains useful when it
        # is attached to the ingredient line that contains it.
        if len(text.split()) < 2 and sum(character.isalnum() for character in text) < 5:
            continue
        box = boxes[index] if index < len(boxes) else []
        rows.append((_box_sort_key(box, index), text, score))
    rows.sort(key=lambda row: row[0])
    texts = [row[1] for row in rows]
    scores = [row[2] for row in rows]
    return {
        "text": " ".join(texts),
        "score": round(sum(scores) / len(scores), 4) if scores else 0.0,
    }


def ocr_frames(
    frame_paths: list[Path],
    lang: str,
    batch_size: int,
    score_threshold: float,
) -> dict[str, object]:
    if not frame_paths:
        raise RuntimeError("no OCR frames were supplied")
    missing = [str(path) for path in frame_paths if not path.is_file()]
    if missing:
        raise RuntimeError(f"OCR frame does not exist: {missing[0]}")

    _configure_paddle_cache()
    try:
        from paddleocr import PaddleOCR
        from paddlex.inference import load_pipeline_config
    except ImportError as error:  # pragma: no cover - exercised in deployments
        raise RuntimeError(
            "PaddleOCR is not installed; install paddlepaddle==3.2.0 and paddleocr"
        ) from error

    # PaddleOCR owns exactly one small detector/recognizer pair for this
    # invocation. The pipeline config batches both screenshots and recognition
    # crops; `predict_iter` streams results without retaining tensors for the
    # whole video. Benchmarks established small as the production accuracy/
    # speed point, so alternative model sizes are intentionally not exposed.
    pipeline_config = load_pipeline_config("OCR")
    pipeline_config["batch_size"] = batch_size
    ocr = PaddleOCR(
        text_detection_model_name="PP-OCRv6_small_det",
        text_recognition_model_name="PP-OCRv6_small_rec",
        device=os.environ.get("MEDIA_OCR_DEVICE", "cpu"),
        use_doc_orientation_classify=False,
        use_doc_unwarping=False,
        use_textline_orientation=False,
        text_recognition_batch_size=batch_size,
        paddlex_config=pipeline_config,
    )
    results = ocr.predict_iter([str(path) for path in frame_paths])
    readings = [
        _paddle_frame_reading(result, score_threshold, lang) for result in results
    ]
    if len(readings) != len(frame_paths):
        raise RuntimeError(
            f"PaddleOCR returned {len(readings)} frame results for {len(frame_paths)} inputs"
        )
    return {
        "engine": "paddleocr",
        "model": "PP-OCRv6_small",
        "model_size": "small",
        "lang": lang,
        "batch_size": batch_size,
        "frames": readings,
    }


def transcribe(audio_path: Path, model_name: str, task: str = "transcribe") -> dict[str, object]:
    try:
        from faster_whisper import WhisperModel
    except ImportError as error:  # pragma: no cover - exercised in deployments
        raise RuntimeError(
            "faster-whisper is not installed; install the media runtime or set MEDIA_PYTHON"
        ) from error

    cache_dir = os.environ.get("MEDIA_MODEL_CACHE", "") or None
    kwargs: dict[str, object] = {
        "device": "cpu",
        "compute_type": os.environ.get("MEDIA_WHISPER_COMPUTE_TYPE", "int8"),
        "download_root": cache_dir,
    }
    try:
        model = WhisperModel(model_name, **kwargs)
    except (ValueError, RuntimeError):
        # Some CPUs do not expose the integer kernels used by int8. The
        # float32 fallback remains local and is more portable than failing the
        # entire description/OCR import.
        kwargs["compute_type"] = "float32"
        model = WhisperModel(model_name, **kwargs)

    segments, info = model.transcribe(
        str(audio_path),
        beam_size=5,
        vad_filter=True,
        condition_on_previous_text=False,
        temperature=0.0,
        task=task,
    )
    pieces: list[str] = []
    structured: list[dict[str, object]] = []
    for segment in segments:
        text = " ".join(segment.text.split())
        if not text:
            continue
        pieces.append(text)
        structured.append(
            {
                "start": round(float(segment.start), 2),
                "end": round(float(segment.end), 2),
                "text": text,
            }
        )
    return {
        "text": " ".join(pieces),
        "language": getattr(info, "language", None),
        "segments": structured,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    transcribe_parser = subparsers.add_parser("transcribe")
    transcribe_parser.add_argument("--model", default=os.environ.get("MEDIA_WHISPER_MODEL", "base"))
    transcribe_parser.add_argument(
        "--task",
        default=os.environ.get("MEDIA_WHISPER_TASK", "transcribe"),
        choices=["transcribe", "translate"],
    )
    transcribe_parser.add_argument("audio", type=Path)

    ocr_parser = subparsers.add_parser("ocr")
    ocr_parser.add_argument(
        "--lang", default=os.environ.get("MEDIA_OCR_LANG", "en")
    )
    ocr_parser.add_argument(
        "--batch-size",
        type=int,
        default=_env_int("MEDIA_OCR_BATCH_SIZE", 8),
    )
    ocr_parser.add_argument(
        "--score-threshold",
        type=float,
        default=float(os.environ.get("MEDIA_OCR_SCORE_THRESHOLD", "0.70")),
    )
    ocr_parser.add_argument("frames", nargs="+", type=Path)

    args = parser.parse_args()
    if args.command == "transcribe":
        if not args.audio.is_file():
            raise RuntimeError(f"audio file does not exist: {args.audio}")
        print(json.dumps(transcribe(args.audio, args.model, args.task), ensure_ascii=False))
        return
    if args.command == "ocr":
        if not 0.0 <= args.score_threshold <= 1.0:
            raise RuntimeError("--score-threshold must be between 0 and 1")
        print(
            json.dumps(
                ocr_frames(
                    args.frames,
                    args.lang,
                    max(1, args.batch_size),
                    args.score_threshold,
                ),
                ensure_ascii=False,
            )
        )
        return
    raise RuntimeError("unknown command")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:  # Keep stdout a single machine-readable line.
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        raise
