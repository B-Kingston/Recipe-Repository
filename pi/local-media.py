#!/usr/bin/env python3
"""Local CPU speech-to-text and OCR helper for the recipe video importer.

Two subcommands read media the Rust app extracted with ffmpeg:

* `transcribe` runs faster-whisper over a mono 16 kHz WAV.
* `ocr` runs PaddleOCR PP-OCRv6 small over screenshots.

Both run entirely on this machine; no audio or frame is sent to an AI API.
Each prints one JSON document on stdout. `serve` keeps the same two commands
alive behind a JSON-lines protocol (see `serve`) so the interpreter, the
imports, and the loaded models are paid for once per worker instead of once
per import. The one-shot subcommands and the worker share every code path
below, so the benchmarks measure what production runs.
"""

from __future__ import annotations

import argparse
import json
import os
import select
import sys
import time
from pathlib import Path
from typing import IO

# Models stay loaded for the lifetime of the process, keyed by every parameter
# that changes what gets built. A one-shot run uses each entry once; the
# worker reuses them across requests.
_MODELS: dict[tuple[object, ...], object] = {}
# Seconds spent importing libraries and building models so far; a request's
# log line reports the share it paid (0 once the models are warm).
_BUILD_SECONDS = [0.0]


def _remember(key: tuple[object, ...], model: object, started: float) -> None:
    _MODELS[key] = model
    _BUILD_SECONDS[0] += time.perf_counter() - started


# The command kind this process has loaded models for (see `_handle`).
_KIND: list[str | None] = [None]

# A lone short word is usually background texture. A confident one is a real
# label (`MISO`, `salt`, `skin`, `like` score 0.999-1.0; the junk `SLS` 0.879).
_LONE_WORD_MIN_ALNUM = 5
_LONE_WORD_MIN_SCORE = 0.95

# Boxes share a visual row when their vertical extents overlap by at least
# this fraction of the smaller box's height.
_ROW_OVERLAP = 0.5


def _env_int(name: str, default: int) -> int:
    try:
        return max(1, int(os.environ.get(name, str(default))))
    except ValueError:
        return default


def _env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name, "").strip().lower()
    if not value:
        return default
    return value not in {"0", "false", "off", "no"}


def _log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


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


def _box_extent(box: object, fallback: int) -> tuple[float, float, float, float]:
    """Return (left, top, right, bottom) of a Paddle box.

    `rec_boxes` rows are `[x1, y1, x2, y2]`; `rec_polys` rows are corner
    lists. A missing or unreadable box gets a synthetic zero-height slot at
    its index, so such boxes keep their input order and never merge into a
    row (the overlap test needs a positive height on both boxes).
    """
    try:
        points = list(box)  # type: ignore[call-overload]
        if points and not hasattr(points[0], "__len__"):
            values = [float(value) for value in points]
            return (min(values[0], values[2]), min(values[1], values[3]),
                    max(values[0], values[2]), max(values[1], values[3]))
        xs = [float(point[0]) for point in points]
        ys = [float(point[1]) for point in points]
        return (min(xs), min(ys), max(xs), max(ys))
    except (TypeError, ValueError, IndexError):
        top = float(fallback) * 2.0
        return (0.0, top, 0.0, top)


def _is_cjk(character: str) -> bool:
    return (
        "\u3040" <= character <= "\u30ff"  # Hiragana, Katakana
        or "\u1100" <= character <= "\u11ff"  # Hangul Jamo
        or "\uac00" <= character <= "\ud7af"  # Hangul syllables
        or "\u3400" <= character <= "\u4dbf"  # CJK extension A
        or "\u4e00" <= character <= "\u9fff"  # CJK unified ideographs
        or "\uf900" <= character <= "\ufaff"  # CJK compatibility ideographs
    )


def _text_matches_language(text: str, lang: str) -> bool:
    """Reject confident-looking output in a script the selected model cannot read.

    PP-OCR can occasionally interpret food texture as repeated CJK glyphs even
    when the explicit English recognizer is loaded. Confidence alone cannot
    remove that failure because the recognizer is highly certain. Keep CJK for
    the language families that support it and reject CJK-majority lines for
    the normal English/Arabic recipe configurations. Kana and Hangul count as
    CJK, not only Han ideographs, so a Japanese caption misread from texture
    is rejected too.
    """
    letters = [character for character in text if character.isalpha()]
    if not letters:
        return True
    cjk = sum(_is_cjk(character) for character in letters)
    language = lang.strip().lower()
    supports_cjk = language in {"ch", "zh", "ja", "ko"} or language.startswith(
        ("chinese", "japan", "korean")
    )
    return supports_cjk or cjk * 2 <= len(letters)


def _merge_rows(
    boxes: list[tuple[tuple[float, float, float, float], str, float]],
) -> list[tuple[str, list[float]]]:
    """Group boxes into visual rows, top to bottom, each left to right.

    A box joins the first row holding a box it overlaps vertically by at least
    `_ROW_OVERLAP` of the smaller height. Returns (row text, box scores).
    """
    rows: list[list[tuple[tuple[float, float, float, float], str, float]]] = []
    for item in sorted(boxes, key=lambda item: (item[0][1], item[0][0])):
        _, top, _, bottom = item[0]
        for row in rows:
            for (_, other_top, _, other_bottom), _, _ in row:
                overlap = min(bottom, other_bottom) - max(top, other_top)
                smaller = min(bottom - top, other_bottom - other_top)
                if smaller > 0 and overlap >= smaller * _ROW_OVERLAP:
                    break
            else:
                continue
            row.append(item)
            break
        else:
            rows.append([item])

    def centre(row: list[tuple[tuple[float, float, float, float], str, float]]) -> float:
        return sum(item[0][1] + item[0][3] for item in row) / (2 * len(row))

    merged = []
    for row in sorted(rows, key=centre):
        row.sort(key=lambda item: item[0][0])
        merged.append((" ".join(item[1] for item in row), [item[2] for item in row]))
    return merged


def _paddle_frame_reading(
    result: object, score_threshold: float, lang: str
) -> dict[str, object]:
    """Turn one Paddle result into Contract A's frame entry.

    Order matters: a box is dropped for low confidence or a wrong script first;
    the survivors are merged into visual rows; only then does the lone-word
    gate look at a row, so a quantity next to its ingredient keeps both.
    """
    payload = _paddle_result_payload(result)
    texts = payload.get("rec_texts") or []
    scores = payload.get("rec_scores") or []
    geometry = payload.get("rec_boxes")
    if geometry is None or len(geometry) == 0:
        geometry = payload.get("rec_polys") or []
    kept: list[tuple[tuple[float, float, float, float], str, float]] = []
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
        box = geometry[index] if index < len(geometry) else []
        kept.append((_box_extent(box, index), text, score))

    lines: list[dict[str, object]] = []
    all_scores: list[float] = []
    for text, row_scores in _merge_rows(kept):
        score = sum(row_scores) / len(row_scores)
        if (
            len(text.split()) < 2
            and sum(character.isalnum() for character in text) < _LONE_WORD_MIN_ALNUM
            and score < _LONE_WORD_MIN_SCORE
        ):
            continue
        lines.append({"text": text, "score": round(score, 4)})
        all_scores.extend(row_scores)
    return {
        "text": "\n".join(str(line["text"]) for line in lines),
        "score": round(sum(all_scores) / len(all_scores), 4) if all_scores else 0.0,
        "lines": lines,
        "skipped": False,
    }


def _skipped_frame() -> dict[str, object]:
    return {"text": "", "score": 0.0, "lines": [], "skipped": True}


def _ocr_model(batch_size: int) -> object:
    key = ("ocr", batch_size, os.environ.get("MEDIA_OCR_DEVICE", "cpu"))
    model = _MODELS.get(key)
    if model is None:
        started_build = time.perf_counter()
        try:
            from paddleocr import PaddleOCR
            from paddlex.inference import load_pipeline_config
        except ImportError as error:  # pragma: no cover - exercised in deployments
            raise RuntimeError(
                "PaddleOCR is not installed; install paddlepaddle==3.2.0 and paddleocr"
            ) from error
        # PaddleOCR owns exactly one small detector/recognizer pair per
        # process. The pipeline config batches both screenshots and
        # recognition crops; `predict_iter` streams results without retaining
        # tensors for the whole video. Benchmarks established small as the
        # production accuracy/speed point, so alternative model sizes are
        # intentionally not exposed.
        pipeline_config = load_pipeline_config("OCR")
        pipeline_config["batch_size"] = batch_size
        model = PaddleOCR(
            text_detection_model_name="PP-OCRv6_small_det",
            text_recognition_model_name="PP-OCRv6_small_rec",
            device=key[2],
            use_doc_orientation_classify=False,
            use_doc_unwarping=False,
            use_textline_orientation=False,
            text_recognition_batch_size=batch_size,
            paddlex_config=pipeline_config,
        )
        _remember(key, model, started_build)
    return model


def _detector(side: int) -> object:
    """The detection-only model behind the text-presence gate.

    It shares the recognizer's detector weights but resizes the longer side to
    `side` px, which costs a fraction of the full pipeline's detection.
    """
    key = ("det", side, os.environ.get("MEDIA_OCR_DEVICE", "cpu"))
    model = _MODELS.get(key)
    if model is None:
        started_build = time.perf_counter()
        try:
            from paddleocr import TextDetection
        except ImportError as error:  # pragma: no cover - exercised in deployments
            raise RuntimeError(
                "PaddleOCR is not installed; install paddlepaddle==3.2.0 and paddleocr"
            ) from error
        model = TextDetection(
            model_name="PP-OCRv6_small_det",
            device=key[2],
            limit_type="max",
            limit_side_len=side,
        )
        _remember(key, model, started_build)
    return model


def _frames_with_text(frame_paths: list[Path], batch_size: int, side: int) -> list[bool]:
    """Detection only: True for each frame where the detector finds a box."""
    detector = _detector(side)
    results = detector.predict([str(path) for path in frame_paths], batch_size=batch_size)  # type: ignore[attr-defined]
    has_text = [
        bool(_paddle_result_payload(result).get("dt_polys")) for result in results
    ]
    if len(has_text) != len(frame_paths):
        raise RuntimeError(
            f"text detection returned {len(has_text)} frame results for {len(frame_paths)} inputs"
        )
    return has_text


def ocr_frames(
    frame_paths: list[Path],
    lang: str,
    batch_size: int,
    score_threshold: float,
    gate: bool | None = None,
    gate_side: int | None = None,
) -> dict[str, object]:
    if not frame_paths:
        raise RuntimeError("no OCR frames were supplied")
    missing = [str(path) for path in frame_paths if not path.is_file()]
    if missing:
        raise RuntimeError(f"OCR frame does not exist: {missing[0]}")
    if not 0.0 <= score_threshold <= 1.0:
        raise RuntimeError("score threshold must be between 0 and 1")
    batch_size = max(1, batch_size)
    if gate is None:
        gate = _env_bool("MEDIA_OCR_TEXT_GATE", True)
    if gate_side is None:
        gate_side = _env_int("MEDIA_OCR_GATE_SIDE", 640)

    _configure_paddle_cache()
    has_text = (
        _frames_with_text(frame_paths, batch_size, gate_side)
        if gate
        else [True] * len(frame_paths)
    )
    pending = [path for path, found in zip(frame_paths, has_text) if found]
    readings: list[dict[str, object]] = []
    if pending:
        # Frames the gate rejected never reach the recognizer; the stream
        # below yields exactly one result per pending path, in order.
        results = _ocr_model(batch_size).predict_iter([str(path) for path in pending])  # type: ignore[attr-defined]
        readings = [
            _paddle_frame_reading(result, score_threshold, lang) for result in results
        ]
    if len(readings) != len(pending):
        raise RuntimeError(
            f"PaddleOCR returned {len(readings)} frame results for {len(pending)} inputs"
        )
    stream = iter(readings)
    frames = [next(stream) if found else _skipped_frame() for found in has_text]
    return {
        "engine": "paddleocr",
        "model": "PP-OCRv6_small",
        "model_size": "small",
        "lang": lang,
        "batch_size": batch_size,
        "gate": {
            "enabled": gate,
            "checked": len(frame_paths) if gate else 0,
            "skipped": has_text.count(False),
        },
        "frames": frames,
    }


def _whisper_model(model_name: str) -> object:
    compute_type = os.environ.get("MEDIA_WHISPER_COMPUTE_TYPE", "int8")
    key = ("whisper", model_name, compute_type)
    pipeline = _MODELS.get(key)
    if pipeline is None:
        started_build = time.perf_counter()
        try:
            from faster_whisper import BatchedInferencePipeline, WhisperModel
        except ImportError as error:  # pragma: no cover - exercised in deployments
            raise RuntimeError(
                "faster-whisper is not installed; install the media runtime or set MEDIA_PYTHON"
            ) from error
        cache_dir = os.environ.get("MEDIA_MODEL_CACHE", "") or None
        kwargs: dict[str, object] = {
            "device": "cpu",
            "compute_type": compute_type,
            "download_root": cache_dir,
        }
        try:
            model = WhisperModel(model_name, **kwargs)
        except (ValueError, RuntimeError):
            # Some CPUs do not expose the integer kernels used by int8. The
            # float32 fallback remains local and is more portable than failing
            # the entire description/OCR import.
            kwargs["compute_type"] = "float32"
            model = WhisperModel(model_name, **kwargs)
        pipeline = BatchedInferencePipeline(model=model)
        _remember(key, pipeline, started_build)
    return pipeline


def transcribe(
    audio_path: Path,
    model_name: str,
    task: str = "transcribe",
    batch_size: int | None = None,
) -> dict[str, object]:
    if not audio_path.is_file():
        raise RuntimeError(f"audio file does not exist: {audio_path}")
    if task not in {"transcribe", "translate"}:
        raise RuntimeError(f"unknown transcription task: {task}")
    if batch_size is None:
        batch_size = _env_int("MEDIA_WHISPER_BATCH_SIZE", 8)
    # Batched decoding runs the VAD-cut speech chunks through the model
    # together and is faster and, measured, slightly more accurate than the
    # sequential beam-5 path. The caption must never be fed back as an
    # initial prompt or hotwords: both made the transcript worse. Batched
    # chunks span up to 30 s and timestamp decoding dropped words, so word
    # timings split each chunk into sentence segments instead.
    segments, info = _whisper_model(model_name).transcribe(  # type: ignore[attr-defined]
        str(audio_path),
        batch_size=max(1, batch_size),
        beam_size=1,
        vad_filter=True,
        temperature=0.0,
        task=task,
        word_timestamps=True,
    )
    pieces: list[str] = []
    structured: list[dict[str, object]] = []
    for segment in segments:
        text = " ".join(segment.text.split())
        if not text:
            continue
        pieces.append(text)
        structured.extend(_sentence_segments(segment, text))
    return {
        "text": " ".join(pieces),
        "language": getattr(info, "language", None),
        "segments": structured,
    }


# A pause this long between two words also ends a timed segment.
_SEGMENT_PAUSE_SECONDS = 1.0
# Speech without sentence marks (common in Arabic output) still splits: after
# this long a clause mark ends the segment, and after the hard limit any word.
_SEGMENT_SOFT_SECONDS = 5.0
_SEGMENT_HARD_SECONDS = 8.0
_SENTENCE_MARKS = (".", "?", "!", "\u061f", "\u06d4", "\u3002", "\uff1f", "\uff01")
_CLAUSE_MARKS = (",", ";", ":", "\u060c", "\u061b", "\u3001", "\uff0c")


def _sentence_segments(segment: object, text: str) -> list[dict[str, object]]:
    """Split one decoded chunk into timed sentences with its word timings.

    A sentence ends after a word with a sentence mark, before a pause of
    `_SEGMENT_PAUSE_SECONDS`, after a clause mark once it has run
    `_SEGMENT_SOFT_SECONDS`, and after any word once it has run
    `_SEGMENT_HARD_SECONDS`. A chunk without word timings stays whole.
    """
    words = [word for word in (getattr(segment, "words", None) or []) if word.word.strip()]
    if not words:
        return [
            {
                "start": round(float(segment.start), 2),  # type: ignore[attr-defined]
                "end": round(float(segment.end), 2),  # type: ignore[attr-defined]
                "text": text,
            }
        ]
    sentences: list[dict[str, object]] = []
    current: list[object] = []
    for index, word in enumerate(words):
        current.append(word)
        following = words[index + 1] if index + 1 < len(words) else None
        mark = word.word.strip()
        length = word.end - current[0].start  # type: ignore[attr-defined]
        ends_sentence = (
            mark.endswith(_SENTENCE_MARKS)
            or (length >= _SEGMENT_SOFT_SECONDS and mark.endswith(_CLAUSE_MARKS))
            or length >= _SEGMENT_HARD_SECONDS
        )
        pause = following is not None and following.start - word.end >= _SEGMENT_PAUSE_SECONDS
        if following is None or ends_sentence or pause:
            sentence = " ".join("".join(item.word for item in current).split())  # type: ignore[attr-defined]
            if sentence:
                sentences.append(
                    {
                        "start": round(float(current[0].start), 2),  # type: ignore[attr-defined]
                        "end": round(float(word.end), 2),
                        "text": sentence,
                    }
                )
            current = []
    return sentences


def _protect_stdout() -> IO[str]:
    """Reserve the real stdout for JSON and point fd 1 at stderr.

    PaddlePaddle's C++ runtime writes diagnostics straight to fd 1, bypassing
    `sys.stdout`; any such line inside a response would corrupt it. A private
    duplicate of the original stdout carries the output; everything else that
    reaches fd 1 lands in the log stream instead.
    """
    sys.stdout.flush()
    channel = os.fdopen(os.dup(1), "w", encoding="utf-8", buffering=1, newline="\n")
    os.dup2(2, 1)
    return channel


def _handle(request: dict[str, object]) -> dict[str, object]:
    command = request.get("command")
    started = time.perf_counter()
    # CTranslate2 (Whisper) and PaddlePaddle crash the interpreter when both
    # are loaded in one process (seen: Whisper first, then Paddle: SIGSEGV
    # with no response). The Rust client runs one worker per kind, so a worker
    # refuses the other kind instead of dying mid-request.
    if command in {"transcribe", "ocr"}:
        if _KIND[0] not in (None, command):
            raise RuntimeError(
                f"this worker already serves {_KIND[0]} requests; start another for {command}"
            )
        _KIND[0] = command
    if command == "transcribe":
        audio = request.get("audio")
        if not isinstance(audio, str) or not audio:
            raise RuntimeError("transcribe needs an audio path")
        built = _BUILD_SECONDS[0]
        result = transcribe(
            Path(audio),
            str(request.get("model") or os.environ.get("MEDIA_WHISPER_MODEL", "base")),
            str(request.get("task") or os.environ.get("MEDIA_WHISPER_TASK", "transcribe")),
            _optional_int(request.get("batchSize")),
        )
        segments = result["segments"]
        _log(
            f"transcribe: {len(segments)} segments in "  # type: ignore[arg-type]
            f"{time.perf_counter() - started:.2f}s "
            f"(import and model build {_BUILD_SECONDS[0] - built:.2f}s)"
        )
        return result
    if command == "ocr":
        frames = request.get("frames")
        if (
            not isinstance(frames, list)
            or not frames
            or not all(isinstance(frame, str) for frame in frames)
        ):
            raise RuntimeError("ocr needs a non-empty list of frame paths")
        built = _BUILD_SECONDS[0]
        threshold = request.get("scoreThreshold")
        result = ocr_frames(
            [Path(frame) for frame in frames],
            str(request.get("lang") or os.environ.get("MEDIA_OCR_LANG", "en")),
            _optional_int(request.get("batchSize")) or _env_int("MEDIA_OCR_BATCH_SIZE", 8),
            float(threshold)  # type: ignore[arg-type]
            if threshold is not None
            else float(os.environ.get("MEDIA_OCR_SCORE_THRESHOLD", "0.70")),
            gate=request["gate"] if isinstance(request.get("gate"), bool) else None,
            gate_side=_optional_int(request.get("gateSide")),
        )
        gate = result["gate"]
        _log(
            f"ocr: {len(frames)} frames, gate skipped {gate['skipped']}/{gate['checked']}, "  # type: ignore[index]
            f"{time.perf_counter() - started:.2f}s "
            f"(import and model build {_BUILD_SECONDS[0] - built:.2f}s)"
        )
        return result
    raise RuntimeError(f"unknown command: {command!r}")


def _optional_int(value: object) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    return value if value > 0 else None


def serve(channel: IO[str]) -> None:
    """JSON-lines worker: one request line in, one response line out.

    Request: `{"id": <any>, "command": "transcribe"|"ocr", ...parameters}`.
    Response: `{"id": <echoed>, "ok": true, "result": {...}}` or
    `{"id": <echoed>, "ok": false, "error": "<message>"}`. A failing request
    produces an error response and the worker keeps running. The worker exits
    when stdin closes or after `MEDIA_WORKER_IDLE_SECONDS` without a request.
    """
    idle_seconds = float(_env_int("MEDIA_WORKER_IDLE_SECONDS", 600))
    pending = b""
    while True:
        while b"\n" not in pending:
            ready, _, _ = select.select([0], [], [], idle_seconds)
            if not ready:
                _log(f"worker idle for {idle_seconds:.0f}s; exiting")
                return
            chunk = os.read(0, 1 << 16)
            if not chunk:
                return
            pending += chunk
        line, _, pending = pending.partition(b"\n")
        if not line.strip():
            continue
        request_id: object = None
        try:
            request = json.loads(line)
            if not isinstance(request, dict):
                raise RuntimeError("request must be a JSON object")
            request_id = request.get("id")
            response: dict[str, object] = {
                "id": request_id,
                "ok": True,
                "result": _handle(request),
            }
        except Exception as error:  # a bad request must not take the worker down
            _log(f"request failed: {error}")
            response = {"id": request_id, "ok": False, "error": str(error) or type(error).__name__}
        channel.write(json.dumps(response, ensure_ascii=False) + "\n")
        channel.flush()


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
    transcribe_parser.add_argument(
        "--batch-size",
        type=int,
        default=_env_int("MEDIA_WHISPER_BATCH_SIZE", 8),
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

    subparsers.add_parser("serve")

    args = parser.parse_args()
    channel = _protect_stdout()
    if args.command == "serve":
        serve(channel)
        return
    if args.command == "transcribe":
        result = transcribe(args.audio, args.model, args.task, args.batch_size)
    elif args.command == "ocr":
        result = ocr_frames(args.frames, args.lang, args.batch_size, args.score_threshold)
    else:
        raise RuntimeError("unknown command")
    channel.write(json.dumps(result, ensure_ascii=False) + "\n")
    channel.flush()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:  # The Rust caller reads this JSON line from stderr first.
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        raise
