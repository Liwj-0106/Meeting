#!/usr/bin/env python3
"""Meetily's local-only MOSS JSONL worker.

Production mode loads one pinned, locally installed MOSS-Transcribe-Diarize
model during the protocol handshake. It never resolves a Hub model ID and all
Transformers/Hugging Face network access is disabled. The synthetic path is
available only through ``--fixture`` for Rust transport tests.

stdout is protocol-only. Exceptions, paths, prompts, transcripts and model
output are never printed by this module.
"""

from __future__ import annotations

import argparse
import gc
import hashlib
import importlib.util
import json
import math
import os
import re
import sys
import threading
import unicodedata
from pathlib import Path
from types import ModuleType
from typing import Any, Iterable


SCHEMA = 1
CANONICAL_FRAMES_PER_SECOND = 48_000
MAX_LINE_BYTES = 8 * 1024 * 1024
MAX_WINDOW_FRAMES = 90 * CANONICAL_FRAMES_PER_SECOND
MAX_PROMPT_CHARS = 4_096
MAX_SEGMENTS = 4_096
MAX_SEGMENT_CHARS = 16_384
MAX_TOTAL_TEXT_CHARS = 1_048_576
MAX_NEW_TOKENS = 5_120
IDENTIFIER = re.compile(r"^[A-Za-z0-9._:-]{1,128}$")
REVISION = re.compile(r"^[A-Za-z0-9._:/-]{1,128}$")
SPEAKER = re.compile(r"^S[0-9]{1,15}$")
OUTPUT_LOCK = threading.Lock()
BENCHMARK_METRICS_ENV = "MEETILY_MOSS_BENCHMARK_METRICS"

# Updating either upstream artifact requires a code review and a deliberate
# change here. Production never trusts an arbitrary local checkout/revision.
TRUSTED_SOURCE_REPOSITORY = "OpenMOSS/MOSS-Transcribe-Diarize"
TRUSTED_SOURCE_REVISION = "cb765f2b0fe6f7a298aa2002e2281ae693d1f3c3"
TRUSTED_MODEL_REPOSITORY = "OpenMOSS-Team/MOSS-Transcribe-Diarize"
TRUSTED_MODEL_REVISIONS = frozenset(
    {"902e98bcb3db33ac913d3496127b92a8d81f2daa"}
)
TRUSTED_MODEL_WEIGHT_FILE = "model-00000-of-00001.safetensors"
TRUSTED_MODEL_WEIGHT_BYTES = 1_817_113_576
TRUSTED_MODEL_WEIGHT_SHA256 = (
    "9a0ceb4ab7330357db3ff583dba8d83625d5b733b00e1d55d6970e11b07026c4"
)
TRUSTED_SOURCE_FILES = {
    "LICENSE": (
        "c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4"
    ),
    "moss_transcribe_diarize/inference_utils.py": (
        "1d97700b83ed95438be2e3a59529b3726d7bf7f7cdd488ad2097180bad15b86b"
    ),
    "moss_transcribe_diarize/transcript_parser.py": (
        "475c564edc128afde69ae27f1fe8575412b1d6ac8f9d6be38b6c7f4bd16b8412"
    ),
}
TRUSTED_SOURCE_MANIFEST = "meetily-source-manifest.json"
TRUSTED_MODEL_MANIFEST = "meetily-model-manifest.json"
TRUSTED_MODEL_CODE_FILES = {
    "configuration_moss_transcribe_diarize.py": (
        "b4d12b0f4609af69b61c2fe3aa5fbaf476af22278369e4540745bc47d1d37892"
    ),
    "modeling_moss_transcribe_diarize.py": (
        "a01da90fe1f7cb88942b8c56f443e7b4ecd307ed4b4c356ac08d7503dd7422c1"
    ),
    "processing_moss_transcribe_diarize.py": (
        "6f228d22d9379e2f6a6830b18ce7336b22da8267547e96e65545d871d7f48766"
    ),
    "config.json": (
        "2b2b7a6e61334152bdd7ecf8a4da3073b4940a097e193d1d2b22093e77535234"
    ),
    "processor_config.json": (
        "a978c2dd54a65b576c3dae4b654fe9bcbac1184c6db2df0afb2c90fcdc872ae7"
    ),
}
TRUSTED_MODEL_FILES = (
    "added_tokens.json",
    "chat_template.jinja",
    "config.json",
    "configuration_moss_transcribe_diarize.py",
    "generation_config.json",
    "merges.txt",
    TRUSTED_MODEL_WEIGHT_FILE,
    "model.safetensors.index.json",
    "modeling_moss_transcribe_diarize.py",
    "preprocessor_config.json",
    "processing_moss_transcribe_diarize.py",
    "processor_config.json",
    "special_tokens_map.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "vocab.json",
)
REQUIRED_MODEL_FILES = TRUSTED_MODEL_FILES + (
    "model-revision.txt",
    TRUSTED_MODEL_MANIFEST,
)


class AdapterUnavailable(RuntimeError):
    """Safe internal marker. Its message is never returned across JSONL."""


class InvalidInferenceResult(RuntimeError):
    """The model output could not satisfy the canonical response contract."""


def write_message(message: dict[str, Any]) -> None:
    encoded = json.dumps(
        message,
        ensure_ascii=False,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")
    if len(encoded) > MAX_LINE_BYTES or b"\n" in encoded or b"\r" in encoded:
        os._exit(74)
    with OUTPUT_LOCK:
        sys.stdout.buffer.write(encoded + b"\n")
        sys.stdout.buffer.flush()


def read_message() -> dict[str, Any] | None:
    line = sys.stdin.buffer.readline(MAX_LINE_BYTES + 2)
    if not line:
        return None
    if len(line) > MAX_LINE_BYTES + 1 or not line.endswith(b"\n"):
        raise ValueError("invalid_line")
    line = line[:-1]
    if line.endswith(b"\r"):
        line = line[:-1]
    if not line:
        raise ValueError("empty_line")
    value = json.loads(line)
    if not isinstance(value, dict):
        raise ValueError("invalid_message")
    return value


def is_within(path: Path, root: Path) -> bool:
    try:
        candidate = os.path.normcase(os.path.realpath(os.fspath(path)))
        boundary = os.path.normcase(os.path.realpath(os.fspath(root)))
        if os.name == "nt":
            if candidate.startswith("\\\\?\\UNC\\"):
                candidate = "\\\\" + candidate[8:]
            elif candidate.startswith("\\\\?\\"):
                candidate = candidate[4:]
            if boundary.startswith("\\\\?\\UNC\\"):
                boundary = "\\\\" + boundary[8:]
            elif boundary.startswith("\\\\?\\"):
                boundary = boundary[4:]
        return os.path.commonpath([candidate, boundary]) == boundary
    except (OSError, ValueError):
        return False


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _reviewed_text_sha256(path: Path) -> str:
    try:
        normalized = path.read_text(encoding="utf-8").replace("\r\n", "\n")
    except (OSError, UnicodeError) as exc:
        raise AdapterUnavailable("invalid_source") from exc
    if "\r" in normalized:
        raise AdapterUnavailable("invalid_source")
    return hashlib.sha256(normalized.encode("utf-8")).hexdigest()


def _read_small_text(path: Path, limit: int = 512) -> str:
    if not path.is_file() or path.stat().st_size > limit:
        raise AdapterUnavailable("invalid_metadata")
    return path.read_text(encoding="utf-8").strip()


def _load_reviewed_module(name: str, path: Path) -> ModuleType:
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise AdapterUnavailable("invalid_source")
    module = importlib.util.module_from_spec(spec)
    # Dataclasses may resolve the defining module while it is executing.
    sys.modules[name] = module
    try:
        spec.loader.exec_module(module)
    except Exception as exc:
        sys.modules.pop(name, None)
        raise AdapterUnavailable("source_import_failed") from exc
    return module


def _has_forbidden_text_control(
    value: str,
    *,
    allow_newlines: bool = False,
) -> bool:
    return any(
        character == "\x00"
        or (
            unicodedata.category(character) == "Cc"
            and character != "\t"
            and not (allow_newlines and character in {"\r", "\n"})
        )
        for character in value
    )


def _private_diagnostic(code: str) -> None:
    """Emit only a fixed diagnostic code to the transport-private stderr."""

    sys.stderr.write(code + "\n")
    sys.stderr.flush()


def _private_metric(name: str, value: int) -> None:
    allowed = {
        "cuda_peak_allocated_bytes",
        "cuda_peak_reserved_bytes",
        "cuda_total_bytes",
    }
    if (
        os.environ.get(BENCHMARK_METRICS_ENV) == "1"
        and name in allowed
        and isinstance(value, int)
        and value >= 0
    ):
        sys.stderr.write(f"metric_{name}={value}\n")
        sys.stderr.flush()


def validate_prompt(prompt: Any) -> str | None:
    if prompt is None:
        return None
    if (
        not isinstance(prompt, str)
        or not prompt.strip()
        or len(prompt) > MAX_PROMPT_CHARS
        or _has_forbidden_text_control(prompt, allow_newlines=True)
    ):
        raise ValueError("invalid_request")
    return prompt


def _seconds_to_start_frame(seconds: float) -> int:
    return int(math.floor(seconds * CANONICAL_FRAMES_PER_SECOND + 1e-9))


def _seconds_to_end_frame(seconds: float) -> int:
    return int(math.ceil(seconds * CANONICAL_FRAMES_PER_SECOND - 1e-9))


def canonicalize_segments(
    parsed_segments: Iterable[Any],
    window_start_frame: int,
    window_end_frame: int,
) -> list[dict[str, Any]]:
    """Convert model-relative seconds to absolute 48 kHz frame ranges.

    Any malformed, out-of-window, unsorted, overlapping-same-speaker or
    over-limit result rejects the entire window. We do not silently clamp a
    hallucinated timestamp into a valid-looking segment.
    """

    if window_end_frame <= window_start_frame:
        raise InvalidInferenceResult("invalid_window")
    duration_seconds = (
        window_end_frame - window_start_frame
    ) / CANONICAL_FRAMES_PER_SECOND
    output: list[dict[str, Any]] = []
    total_text_chars = 0
    previous_key: tuple[int, int, str] | None = None
    last_end_by_speaker: dict[str, int] = {}

    for segment in parsed_segments:
        if len(output) >= MAX_SEGMENTS:
            raise InvalidInferenceResult("too_many_segments")
        start_seconds = getattr(segment, "start", None)
        end_seconds = getattr(segment, "end", None)
        speaker = getattr(segment, "speaker", None)
        text = getattr(segment, "text", None)
        if (
            isinstance(start_seconds, bool)
            or not isinstance(start_seconds, (int, float))
            or isinstance(end_seconds, bool)
            or not isinstance(end_seconds, (int, float))
            or not math.isfinite(float(start_seconds))
            or not math.isfinite(float(end_seconds))
            or float(start_seconds) < 0.0
            or float(end_seconds) <= float(start_seconds)
            or float(end_seconds) > duration_seconds
            or not isinstance(speaker, str)
            or not SPEAKER.fullmatch(speaker)
            or not isinstance(text, str)
        ):
            raise InvalidInferenceResult("invalid_segment")

        text = text.strip()
        if (
            not text
            or len(text) > MAX_SEGMENT_CHARS
            or _has_forbidden_text_control(text)
        ):
            raise InvalidInferenceResult("invalid_text")
        total_text_chars += len(text)
        if total_text_chars > MAX_TOTAL_TEXT_CHARS:
            raise InvalidInferenceResult("too_much_text")

        start_frame = window_start_frame + _seconds_to_start_frame(float(start_seconds))
        end_frame = window_start_frame + _seconds_to_end_frame(float(end_seconds))
        if (
            start_frame < window_start_frame
            or end_frame > window_end_frame
            or end_frame <= start_frame
        ):
            raise InvalidInferenceResult("invalid_frame_range")
        key = (start_frame, end_frame, speaker)
        if previous_key is not None and previous_key > key:
            raise InvalidInferenceResult("unsorted_segments")
        if start_frame < last_end_by_speaker.get(speaker, 0):
            raise InvalidInferenceResult("same_speaker_overlap")
        previous_key = key
        last_end_by_speaker[speaker] = end_frame
        output.append(
            {
                "start_frame": start_frame,
                "end_frame": end_frame,
                "speaker": speaker,
                "text": text,
            }
        )
    return output


class LocalMossAdapter:
    """One process-local model/processor pair loaded from pinned D storage."""

    def __init__(
        self,
        portable_root: Path,
        model_root: Path,
        model_revision: str,
    ) -> None:
        self.portable_root = portable_root
        self.model_root = model_root
        self.model_revision = model_revision
        worker_root = Path(__file__).resolve(strict=True).parent
        bundled_source = worker_root / "moss_runtime_source"
        if bundled_source.is_dir():
            self.source_root = bundled_source.resolve(strict=True)
            if not is_within(self.source_root, worker_root):
                raise AdapterUnavailable("source_outside_worker")
        else:
            self.source_root = (
                portable_root / "sources" / "MOSS-Transcribe-Diarize"
            ).resolve(strict=True)
            if not is_within(self.source_root, portable_root):
                raise AdapterUnavailable("source_outside_portable")
        self._model: Any = None
        self._processor: Any = None
        self._device: Any = None
        self._dtype: Any = None
        self._torch: Any = None
        self._inference: ModuleType | None = None
        self._parser: ModuleType | None = None

    def _validate_pinned_artifacts(self) -> None:
        if self.model_revision not in TRUSTED_MODEL_REVISIONS:
            raise AdapterUnavailable("unreviewed_model_revision")
        if _read_small_text(self.model_root / "model-revision.txt") != self.model_revision:
            raise AdapterUnavailable("model_revision_mismatch")
        for name in REQUIRED_MODEL_FILES:
            if not (self.model_root / name).is_file():
                raise AdapterUnavailable("incomplete_model")

        try:
            model_manifest = json.loads(
                _read_small_text(
                    self.model_root / TRUSTED_MODEL_MANIFEST,
                    limit=16_384,
                )
            )
        except (AdapterUnavailable, json.JSONDecodeError) as exc:
            raise AdapterUnavailable("unreviewed_model") from exc
        if (
            not isinstance(model_manifest, dict)
            or model_manifest.get("schema") != 1
            or model_manifest.get("repository") != TRUSTED_MODEL_REPOSITORY
            or model_manifest.get("revision") != self.model_revision
            or model_manifest.get("weight_bytes") != TRUSTED_MODEL_WEIGHT_BYTES
            or model_manifest.get("weight_sha256") != TRUSTED_MODEL_WEIGHT_SHA256
            or model_manifest.get("files") != list(TRUSTED_MODEL_FILES)
        ):
            raise AdapterUnavailable("model_manifest_mismatch")

        weight = (self.model_root / TRUSTED_MODEL_WEIGHT_FILE).resolve(strict=True)
        if (
            not is_within(weight, self.model_root)
            or weight.stat().st_size != TRUSTED_MODEL_WEIGHT_BYTES
            or _sha256(weight) != TRUSTED_MODEL_WEIGHT_SHA256
        ):
            raise AdapterUnavailable("model_weight_hash_mismatch")
        for name, expected_hash in TRUSTED_MODEL_CODE_FILES.items():
            if _sha256(self.model_root / name) != expected_hash:
                raise AdapterUnavailable("model_code_hash_mismatch")

        try:
            source_manifest = json.loads(
                _read_small_text(
                    self.source_root / TRUSTED_SOURCE_MANIFEST,
                    limit=4_096,
                )
            )
        except (AdapterUnavailable, json.JSONDecodeError) as exc:
            raise AdapterUnavailable("unreviewed_source") from exc
        if (
            not isinstance(source_manifest, dict)
            or source_manifest.get("schema") != 1
            or source_manifest.get("repository") != TRUSTED_SOURCE_REPOSITORY
            or source_manifest.get("revision") != TRUSTED_SOURCE_REVISION
            or source_manifest.get("files") != TRUSTED_SOURCE_FILES
        ):
            raise AdapterUnavailable("source_revision_mismatch")
        for relative, expected_hash in TRUSTED_SOURCE_FILES.items():
            source = (self.source_root / relative).resolve(strict=True)
            if (
                not is_within(source, self.source_root)
                or _reviewed_text_sha256(source) != expected_hash
            ):
                raise AdapterUnavailable("source_hash_mismatch")

    @staticmethod
    def _offline_environment(portable_root: Path) -> None:
        cache_root = portable_root / "app-data" / "cache"
        cache_paths = {
            "huggingface": cache_root / "huggingface",
            "huggingface_modules": cache_root / "huggingface" / "modules",
            "transformers": cache_root / "huggingface" / "transformers",
            "torch": cache_root / "torch",
            "torch_inductor": cache_root / "torch-inductor",
            "torch_extensions": cache_root / "torch-extensions",
            "numba": cache_root / "numba",
            "triton": cache_root / "triton",
            "cuda": cache_root / "cuda",
            "pip": cache_root / "pip",
            "profile": cache_root / "moss-runtime-profile",
            "appdata": cache_root / "moss-runtime-profile" / "AppData" / "Roaming",
            "localappdata": cache_root / "moss-runtime-profile" / "AppData" / "Local",
            "matplotlib": cache_root / "moss-runtime-profile" / "matplotlib",
        }
        for name, path in cache_paths.items():
            path.mkdir(parents=True, exist_ok=True)
            canonical = path.resolve(strict=True)
            if not is_within(canonical, portable_root):
                raise AdapterUnavailable(f"cache_outside_portable:{name}")
            cache_paths[name] = canonical
        settings = {
            "HF_HUB_OFFLINE": "1",
            "TRANSFORMERS_OFFLINE": "1",
            "HF_DATASETS_OFFLINE": "1",
            "HF_HUB_DISABLE_TELEMETRY": "1",
            "HF_HUB_DISABLE_PROGRESS_BARS": "1",
            "TRANSFORMERS_VERBOSITY": "error",
            "TOKENIZERS_PARALLELISM": "false",
            "HF_HOME": str(cache_paths["huggingface"]),
            "HF_MODULES_CACHE": str(cache_paths["huggingface_modules"]),
            "TRANSFORMERS_CACHE": str(cache_paths["transformers"]),
            "TORCH_HOME": str(cache_paths["torch"]),
            "TORCHINDUCTOR_CACHE_DIR": str(cache_paths["torch_inductor"]),
            "TORCH_EXTENSIONS_DIR": str(cache_paths["torch_extensions"]),
            "NUMBA_CACHE_DIR": str(cache_paths["numba"]),
            "TRITON_CACHE_DIR": str(cache_paths["triton"]),
            "CUDA_CACHE_PATH": str(cache_paths["cuda"]),
            "PIP_CACHE_DIR": str(cache_paths["pip"]),
            "XDG_CACHE_HOME": str(cache_root),
            "USERPROFILE": str(cache_paths["profile"]),
            "HOME": str(cache_paths["profile"]),
            "APPDATA": str(cache_paths["appdata"]),
            "LOCALAPPDATA": str(cache_paths["localappdata"]),
            "MPLCONFIGDIR": str(cache_paths["matplotlib"]),
        }
        os.environ.update(settings)

    def load(self) -> None:
        self._validate_pinned_artifacts()
        self._offline_environment(self.portable_root)
        try:
            import torch
            from transformers import AutoModelForCausalLM, AutoProcessor
        except Exception as exc:
            raise AdapterUnavailable("dependency_unavailable") from exc

        inference_path = self.source_root / next(
            relative
            for relative in TRUSTED_SOURCE_FILES
            if relative.endswith("inference_utils.py")
        )
        parser_path = self.source_root / next(
            relative
            for relative in TRUSTED_SOURCE_FILES
            if relative.endswith("transcript_parser.py")
        )
        inference = _load_reviewed_module("meetily_moss_inference_utils", inference_path)
        parser = _load_reviewed_module("meetily_moss_transcript_parser", parser_path)

        device = torch.device("cpu")
        dtype = torch.float32
        if torch.cuda.is_available():
            device = torch.device("cuda:0")
            dtype = (
                torch.bfloat16
                if torch.cuda.is_bf16_supported()
                else torch.float16
            )

        def load_pair(target_device: Any, target_dtype: Any) -> tuple[Any, Any]:
            model = None
            last_error: Exception | None = None
            for attention in ("sdpa", "eager"):
                try:
                    model = AutoModelForCausalLM.from_pretrained(
                        str(self.model_root),
                        trust_remote_code=True,
                        local_files_only=True,
                        dtype=target_dtype,
                        attn_implementation=attention,
                    )
                    if attention == "eager":
                        _private_diagnostic(
                            "warning=attention_eager_long_window_oom_risk"
                        )
                    break
                except Exception as exc:
                    last_error = exc
                    oom_type = getattr(torch, "OutOfMemoryError", None)
                    if (
                        target_device.type == "cuda"
                        and oom_type is not None
                        and isinstance(exc, oom_type)
                    ):
                        raise
                    if attention == "sdpa":
                        _private_diagnostic("warning=attention_sdpa_unavailable")
                        gc.collect()
                        if target_device.type == "cuda":
                            torch.cuda.empty_cache()
                        continue
                    raise
            if model is None:
                raise AdapterUnavailable("model_load_failed") from last_error
            processor = AutoProcessor.from_pretrained(
                str(self.model_root),
                trust_remote_code=True,
                local_files_only=True,
                fix_mistral_regex=True,
            )
            return model.to(target_device).eval(), processor

        try:
            model, processor = load_pair(device, dtype)
        except Exception as exc:
            oom_type = getattr(torch, "OutOfMemoryError", None)
            is_cuda_oom = device.type == "cuda" and oom_type is not None and isinstance(exc, oom_type)
            if not is_cuda_oom:
                raise AdapterUnavailable("model_load_failed") from exc
            try:
                torch.cuda.empty_cache()
                device = torch.device("cpu")
                dtype = torch.float32
                model, processor = load_pair(device, dtype)
            except Exception as cpu_exc:
                raise AdapterUnavailable("model_load_failed") from cpu_exc

        self._torch = torch
        self._model = model
        self._processor = processor
        self._device = device
        self._dtype = dtype
        self._inference = inference
        self._parser = parser

    @property
    def loaded(self) -> bool:
        return self._model is not None and self._processor is not None

    def runtime_metadata(self) -> dict[str, str]:
        if not self.loaded or self._device is None or self._dtype is None:
            raise AdapterUnavailable("adapter_not_loaded")
        device = self._device.type
        if device == "cuda":
            device = f"cuda:{self._device.index or 0}"
        dtype = str(self._dtype).removeprefix("torch.")
        if not re.fullmatch(r"[a-z0-9_:.-]{1,32}", device + ":" + dtype):
            raise AdapterUnavailable("invalid_runtime_metadata")
        return {"backend": "transformers", "device": device, "dtype": dtype}

    def reset_benchmark_metrics(self) -> None:
        if (
            os.environ.get(BENCHMARK_METRICS_ENV) == "1"
            and self._device is not None
            and self._device.type == "cuda"
        ):
            try:
                self._torch.cuda.reset_peak_memory_stats(self._device)
            except Exception:
                _private_diagnostic("warning=benchmark_metric_unavailable")

    def emit_benchmark_metrics(self) -> None:
        if (
            os.environ.get(BENCHMARK_METRICS_ENV) != "1"
            or self._device is None
            or self._device.type != "cuda"
        ):
            return
        try:
            self._torch.cuda.synchronize(self._device)
            _private_metric(
                "cuda_peak_allocated_bytes",
                int(self._torch.cuda.max_memory_allocated(self._device)),
            )
            _private_metric(
                "cuda_peak_reserved_bytes",
                int(self._torch.cuda.max_memory_reserved(self._device)),
            )
            _private_metric(
                "cuda_total_bytes",
                int(self._torch.cuda.get_device_properties(self._device).total_memory),
            )
        except Exception:
            _private_diagnostic("warning=benchmark_metric_unavailable")

    def transcribe(
        self,
        audio_path: Path,
        prompt: str | None,
        window_start_frame: int,
        window_end_frame: int,
    ) -> list[dict[str, Any]]:
        if not self.loaded or self._inference is None or self._parser is None:
            raise AdapterUnavailable("adapter_not_loaded")
        messages = self._inference.build_transcription_messages(
            audio_path,
            prompt if prompt is not None else self._inference.DEFAULT_PROMPT,
        )
        result = self._inference.generate_transcription(
            self._model,
            self._processor,
            messages,
            max_length=131_072,
            max_new_tokens=MAX_NEW_TOKENS,
            do_sample=False,
            device=self._device,
            dtype=self._dtype,
        )
        raw_text = result.get("text")
        if not isinstance(raw_text, str):
            raise InvalidInferenceResult("missing_text")
        parsed = self._parser.parse_transcript(raw_text)
        if raw_text.strip() and not parsed:
            raise InvalidInferenceResult("unparseable_text")
        return canonicalize_segments(parsed, window_start_frame, window_end_frame)


class Worker:
    def __init__(self, args: argparse.Namespace) -> None:
        self.fixture = bool(args.fixture)
        self.portable_root = Path(args.portable_root).resolve(strict=True)
        self.audio_root = Path(args.audio_root).resolve(strict=True)
        self.model_root = Path(args.model_root).resolve(strict=False)
        self.model_revision = args.model_revision
        if not self.portable_root.is_dir() or not self.audio_root.is_dir():
            raise ValueError("invalid_root")
        if not is_within(self.audio_root, self.portable_root):
            raise ValueError("audio_root_outside_portable_root")
        if not is_within(self.model_root, self.portable_root):
            raise ValueError("model_root_outside_portable_root")
        if not REVISION.fullmatch(self.model_revision):
            raise ValueError("invalid_model_revision")
        self.active: dict[str, threading.Event] = {}
        self.active_lock = threading.Lock()
        self.adapter: LocalMossAdapter | None = None

    def initialize(self) -> tuple[str, str | None, str]:
        if self.fixture:
            return "ready", None, "fixture-v1"
        if not self.model_root.is_dir() or not all(
            (self.model_root / name).is_file() for name in REQUIRED_MODEL_FILES
        ):
            return "model_not_installed", "model_not_installed", "moss-local-v1"
        try:
            adapter = LocalMossAdapter(
                self.portable_root,
                self.model_root.resolve(strict=True),
                self.model_revision,
            )
            adapter.load()
        except Exception:
            return "unavailable", "runtime_unavailable", "moss-local-v1"
        self.adapter = adapter
        return "ready", None, "moss-local-v1"

    def handshake(self, message: dict[str, Any]) -> None:
        if set(message) != {"type", "schema"} or message.get("schema") != SCHEMA:
            raise ValueError("invalid_handshake")
        status, error_code, worker_revision = self.initialize()
        response: dict[str, Any] = {
            "type": "handshake",
            "schema": SCHEMA,
            "status": status,
            "worker_revision": worker_revision,
            "model_revision": self.model_revision,
            "max_in_flight": 1,
        }
        if error_code is not None:
            response["error_code"] = error_code
        if status == "ready" and self.adapter is not None:
            response.update(self.adapter.runtime_metadata())
        write_message(response)

    def validate_request(self, request: Any) -> dict[str, Any]:
        required = {
            "schema",
            "job",
            "session",
            "window_start_frame",
            "window_end_frame",
            "audio_path",
            "model_revision",
        }
        optional = {"prompt"}
        if not isinstance(request, dict) or not required.issubset(request):
            raise ValueError("invalid_request")
        if not set(request).issubset(required | optional):
            raise ValueError("invalid_request")
        if request["schema"] != SCHEMA:
            raise ValueError("invalid_request")
        for field in ("job", "session"):
            if not isinstance(request[field], str) or not IDENTIFIER.fullmatch(request[field]):
                raise ValueError("invalid_request")
        if request["model_revision"] != self.model_revision:
            raise ValueError("invalid_request")
        start = request["window_start_frame"]
        end = request["window_end_frame"]
        if (
            not isinstance(start, int)
            or isinstance(start, bool)
            or not isinstance(end, int)
            or isinstance(end, bool)
            or start < 0
            or end <= start
            or end - start > MAX_WINDOW_FRAMES
        ):
            raise ValueError("invalid_request")
        request["prompt"] = validate_prompt(request.get("prompt"))
        if not isinstance(request["audio_path"], str):
            raise ValueError("invalid_audio_path")
        audio_path = Path(request["audio_path"]).resolve(strict=True)
        if (
            not audio_path.is_file()
            or not is_within(audio_path, self.audio_root)
            or audio_path.suffix.lower() not in {".wav", ".flac"}
        ):
            raise ValueError("invalid_audio_path")
        request["audio_path"] = audio_path
        return request

    def start_execute(self, message: dict[str, Any]) -> None:
        if set(message) != {"type", "request"}:
            self.error(None, "invalid_request")
            return
        try:
            request = self.validate_request(message.get("request"))
        except (OSError, ValueError) as error:
            code = "invalid_audio_path" if str(error) == "invalid_audio_path" else "invalid_request"
            job = message.get("request", {}).get("job")
            self.error(job if isinstance(job, str) else None, code)
            return
        if not self.fixture and self.adapter is None:
            self.error(request["job"], "runtime_unavailable")
            return

        cancel_event = threading.Event()
        with self.active_lock:
            if self.active:
                self.error(request["job"], "job_busy")
                return
            self.active[request["job"]] = cancel_event
        target = self.execute_fixture if self.fixture else self.execute_production
        thread = threading.Thread(
            target=target,
            args=(request, cancel_event),
            name="moss-worker-job",
            daemon=True,
        )
        thread.start()

    def _finish_job(
        self,
        job: str,
        cancel_event: threading.Event,
        message: dict[str, Any] | None,
    ) -> None:
        # Serialize completion with cancel acknowledgement. If cancel wins,
        # the generated result is discarded permanently.
        with self.active_lock:
            if self.active.get(job) is not cancel_event:
                return
            if not cancel_event.is_set() and message is not None:
                write_message(message)
            self.active.pop(job, None)

    def execute_production(
        self,
        request: dict[str, Any],
        cancel_event: threading.Event,
    ) -> None:
        job = request["job"]
        message: dict[str, Any] | None = None
        try:
            if cancel_event.is_set() or self.adapter is None:
                return
            self.adapter.reset_benchmark_metrics()
            segments = self.adapter.transcribe(
                request["audio_path"],
                request.get("prompt"),
                request["window_start_frame"],
                request["window_end_frame"],
            )
            if not cancel_event.is_set():
                message = {
                    "type": "result",
                    "response": {
                        "schema": SCHEMA,
                        "job": job,
                        "session": request["session"],
                        "window_start_frame": request["window_start_frame"],
                        "window_end_frame": request["window_end_frame"],
                        "segments": segments,
                    },
                }
        except Exception as exc:
            if not cancel_event.is_set():
                if (
                    os.environ.get(BENCHMARK_METRICS_ENV) == "1"
                    and isinstance(exc, InvalidInferenceResult)
                    and str(exc)
                    in {
                        "invalid_frame_range",
                        "invalid_speaker",
                        "invalid_text",
                        "missing_text",
                        "same_speaker_overlap",
                        "too_many_segments",
                        "too_much_text",
                        "unparseable_text",
                        "unsorted_segments",
                    }
                ):
                    _private_diagnostic(f"error=invalid_result_{exc}")
                message = {
                    "type": "error",
                    "job": job,
                    "code": "inference_failed",
                }
        finally:
            if self.adapter is not None:
                self.adapter.emit_benchmark_metrics()
            self._finish_job(job, cancel_event, message)

    def execute_fixture(self, request: dict[str, Any], cancel_event: threading.Event) -> None:
        job = request["job"]
        message: dict[str, Any] | None = None
        try:
            if request.get("prompt") == "fixture:crash":
                os._exit(71)
            if request.get("prompt") == "fixture:wait" and cancel_event.wait(10.0):
                return
            if request.get("prompt") == "fixture:delay" and cancel_event.wait(2.25):
                return
            if cancel_event.is_set():
                return
            start = request["window_start_frame"]
            end = min(request["window_end_frame"], start + 4_800)
            message = {
                "type": "result",
                "response": {
                    "schema": SCHEMA,
                    "job": job,
                    "session": request["session"],
                    "window_start_frame": request["window_start_frame"],
                    "window_end_frame": request["window_end_frame"],
                    "segments": [
                        {
                            "start_frame": start,
                            "end_frame": end,
                            "speaker": "S01",
                            "text": "synthetic fixture",
                        }
                    ],
                },
            }
        finally:
            self._finish_job(job, cancel_event, message)

    def cancel(self, message: dict[str, Any]) -> None:
        if set(message) != {"type", "job"} or not isinstance(message.get("job"), str):
            self.error(None, "invalid_request")
            return
        job = message["job"]
        with self.active_lock:
            cancel_event = self.active.get(job)
            if cancel_event is not None:
                cancel_event.set()
            write_message({"type": "cancelled", "job": job})

    @staticmethod
    def error(job: str | None, code: str) -> None:
        write_message({"type": "error", "job": job, "code": code})

    def shutdown(self) -> None:
        with self.active_lock:
            for event in self.active.values():
                event.set()
            write_message({"type": "shutdown", "status": "ok"})


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--portable-root", required=True)
    parser.add_argument("--model-root", required=True)
    parser.add_argument("--audio-root", required=True)
    parser.add_argument("--model-revision", required=True)
    parser.add_argument("--fixture", action="store_true")
    return parser.parse_args()


def main() -> int:
    phase = "arguments"
    try:
        args = parse_args()
        phase = "configuration"
        worker = Worker(args)
        phase = "handshake_input"
        handshake = read_message()
        if handshake is None:
            return 0
        if handshake.get("type") != "handshake":
            return 65
        phase = "handshake_dispatch"
        worker.handshake(handshake)

        while True:
            phase = "request_input"
            message = read_message()
            if message is None:
                return 0
            message_type = message.get("type")
            phase = "request_dispatch"
            if message_type == "execute":
                worker.start_execute(message)
            elif message_type == "cancel":
                worker.cancel(message)
            elif message_type == "shutdown" and set(message) == {"type"}:
                worker.shutdown()
                return 0
            else:
                worker.error(None, "invalid_request")
    except OSError:
        _private_diagnostic(f"error=worker_{phase}_io_failed")
        return 65
    except ValueError as exc:
        detail = str(exc)
        if not re.fullmatch(r"[a-z_]{1,64}", detail):
            detail = "invalid"
        _private_diagnostic(f"error=worker_{phase}_{detail}")
        return 65
    except json.JSONDecodeError:
        _private_diagnostic(f"error=worker_{phase}_invalid_json")
        return 65


if __name__ == "__main__":
    raise SystemExit(main())
