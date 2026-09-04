from __future__ import annotations

import hashlib
import json
import os
import shutil
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import contextmanager
from pathlib import Path
from typing import BinaryIO, Iterator

import requests
from huggingface_hub import snapshot_download


REPOSITORY = "OpenMOSS-Team/MOSS-Transcribe-Diarize"
REVISION = "902e98bcb3db33ac913d3496127b92a8d81f2daa"
WEIGHT_FILE = "model-00000-of-00001.safetensors"
WEIGHT_BYTES = 1_817_113_576
WEIGHT_SHA256 = "9a0ceb4ab7330357db3ff583dba8d83625d5b733b00e1d55d6970e11b07026c4"
MODEL_FILES = (
    "added_tokens.json",
    "chat_template.jinja",
    "config.json",
    "configuration_moss_transcribe_diarize.py",
    "generation_config.json",
    "merges.txt",
    WEIGHT_FILE,
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
CODE_HASHES = {
    "configuration_moss_transcribe_diarize.py": "b4d12b0f4609af69b61c2fe3aa5fbaf476af22278369e4540745bc47d1d37892",
    "modeling_moss_transcribe_diarize.py": "a01da90fe1f7cb88942b8c56f443e7b4ecd307ed4b4c356ac08d7503dd7422c1",
    "processing_moss_transcribe_diarize.py": "6f228d22d9379e2f6a6830b18ce7336b22da8267547e96e65545d871d7f48766",
    "config.json": "2b2b7a6e61334152bdd7ecf8a4da3073b4940a097e193d1d2b22093e77535234",
    "processor_config.json": "a978c2dd54a65b576c3dae4b654fe9bcbac1184c6db2df0afb2c90fcdc872ae7",
}
STATE_FILE = ".meetily-managed-download.json"
WEIGHT_URL = (
    f"https://huggingface.co/{REPOSITORY}/resolve/{REVISION}/{WEIGHT_FILE}"
    "?download=true"
)
# Keep write/progress granularity small enough that a slow proxy does not hold
# several minutes of successfully received bytes only in userspace memory.
DOWNLOAD_CHUNK_BYTES = 256 * 1024
PROGRESS_STEP_BYTES = 64 * 1024 * 1024
MAX_DOWNLOAD_ATTEMPTS = 20
RANGE_WORKERS = 8
RANGE_STATE_FILE = ".model-weight-ranges.json"
DOWNLOAD_ETAG = '"d80b48c98025cec60919d2a7dd916c10a5cc67f4c48e1b7a74d6a94323672600"'
ARTIFACT_LOCK_FILE = ".meetily-model-install.lock"


class ArtifactLockUnavailable(RuntimeError):
    pass


def fail(message: str) -> "NoReturn":
    raise SystemExit(message)


def is_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


@contextmanager
def artifact_lock(path: Path) -> Iterator[None]:
    """Hold an OS-released cross-process lock for one model installation."""

    handle: BinaryIO = path.open("a+b", buffering=0)
    locked = False
    try:
        handle.seek(0, os.SEEK_END)
        if handle.tell() == 0:
            handle.write(b"0")
            os.fsync(handle.fileno())
        handle.seek(0)
        try:
            if os.name == "nt":
                import msvcrt

                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl

                fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as exc:
            raise ArtifactLockUnavailable("model artifact is already being installed") from exc
        locked = True
        yield
    finally:
        if locked:
            handle.seek(0)
            if os.name == "nt":
                import msvcrt

                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                import fcntl

                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        handle.close()


def validate_portable_path(path: Path, portable_root: Path) -> Path:
    if not path.is_absolute() or path.drive.upper() == "C:":
        fail("refusing a non-absolute or system-drive path")
    ancestor = path
    while not ancestor.exists():
        if ancestor.parent == ancestor:
            fail("path has no existing portable ancestor")
        ancestor = ancestor.parent
    if not is_within(ancestor.resolve(strict=True), portable_root):
        fail("path escapes the canonical portable root")
    return path


def validate_download_path(path: Path, portable_root: Path) -> Path:
    if not path.is_absolute() or path.drive.upper() == "C:":
        fail("refusing a non-absolute or system-drive download path")
    ancestor = path
    while not ancestor.exists():
        if ancestor.parent == ancestor:
            fail("download path has no existing ancestor")
        ancestor = ancestor.parent
    canonical_ancestor = ancestor.resolve(strict=True)
    temp_value = os.environ.get("TEMP")
    temp_root = Path(temp_value).resolve(strict=True) if temp_value else None
    if not is_within(canonical_ancestor, portable_root) and (
        temp_root is None or not is_within(canonical_ancestor, temp_root)
    ):
        fail("download path escapes portable and task temporary roots")
    return path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(4 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_prefix(path: Path, length: int) -> str:
    digest = hashlib.sha256()
    remaining = length
    with path.open("rb") as source:
        while remaining:
            chunk = source.read(min(4 * 1024 * 1024, remaining))
            if not chunk:
                fail("staged model weight is shorter than its recorded prefix")
            digest.update(chunk)
            remaining -= len(chunk)
    return digest.hexdigest()


def tree_bytes(root: Path) -> int:
    return sum(path.stat().st_size for path in root.rglob("*") if path.is_file())


def download_weight(download_root: Path, target: Path) -> None:
    if target.is_file():
        if target.stat().st_size == WEIGHT_BYTES and sha256(target) == WEIGHT_SHA256:
            print("model_weight_status=already_verified", flush=True)
            return
        fail("existing managed model weight failed integrity validation")

    download_root.mkdir(parents=True, exist_ok=True)
    partial = download_root / f"{WEIGHT_FILE}.part"
    state_path = download_root / RANGE_STATE_FILE
    if partial.exists() and (not partial.is_file() or partial.stat().st_size > WEIGHT_BYTES):
        fail("invalid staged model weight")

    state_lock = threading.Lock()
    stop_event = threading.Event()

    def persist_state(state: dict[str, object]) -> None:
        temporary_state = state_path.with_suffix(".json.tmp")
        with temporary_state.open("w", encoding="utf-8", newline="\n") as destination:
            json.dump(state, destination, indent=2, sort_keys=True)
            destination.write("\n")
            destination.flush()
            os.fsync(destination.fileno())
        os.replace(temporary_state, state_path)

    def build_ranges(prefix: int) -> list[dict[str, int]]:
        missing = WEIGHT_BYTES - prefix
        if missing <= 0:
            return []
        worker_count = min(RANGE_WORKERS, missing)
        base, extra = divmod(missing, worker_count)
        output: list[dict[str, int]] = []
        cursor = prefix
        for index in range(worker_count):
            length = base + (1 if index < extra else 0)
            end = cursor + length - 1
            output.append({"index": index, "start": cursor, "end": end, "next": cursor})
            cursor = end + 1
        return output

    if state_path.is_file():
        try:
            state = json.loads(state_path.read_text("utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            fail(f"invalid range state: {type(exc).__name__}")
        expected_state = {
            "schema": 1,
            "repository": REPOSITORY,
            "revision": REVISION,
            "weight_file": WEIGHT_FILE,
            "weight_bytes": WEIGHT_BYTES,
            "weight_sha256": WEIGHT_SHA256,
            "etag": DOWNLOAD_ETAG,
        }
        if not isinstance(state, dict) or any(
            state.get(key) != value for key, value in expected_state.items()
        ):
            fail("range state does not match the pinned model artifact")
        ranges = state.get("ranges")
        prefix_bytes = state.get("prefix_bytes")
        prefix_hash = state.get("prefix_sha256")
        if (
            not isinstance(ranges, list)
            or not isinstance(prefix_bytes, int)
            or not 0 <= prefix_bytes <= WEIGHT_BYTES
            or not isinstance(prefix_hash, str)
            or len(prefix_hash) != 64
        ):
            fail("range state has an invalid shape")
        if not partial.is_file() or partial.stat().st_size < prefix_bytes:
            fail("range state has no matching staged prefix")
        if sha256_prefix(partial, prefix_bytes) != prefix_hash:
            fail("staged model prefix failed resume validation")
    else:
        prefix_bytes = partial.stat().st_size if partial.exists() else 0
        if prefix_bytes == WEIGHT_BYTES:
            if sha256(partial) != WEIGHT_SHA256:
                fail("completed staged model weight failed integrity validation")
            os.replace(partial, target)
            print(f"model_weight_verified_bytes={WEIGHT_BYTES}", flush=True)
            return
        prefix_hash = sha256_prefix(partial, prefix_bytes) if prefix_bytes else hashlib.sha256(b"").hexdigest()
        ranges = build_ranges(prefix_bytes)
        state = {
            "schema": 1,
            "repository": REPOSITORY,
            "revision": REVISION,
            "weight_file": WEIGHT_FILE,
            "weight_bytes": WEIGHT_BYTES,
            "weight_sha256": WEIGHT_SHA256,
            "etag": DOWNLOAD_ETAG,
            "prefix_bytes": prefix_bytes,
            "prefix_sha256": prefix_hash,
            "ranges": ranges,
            "status": "downloading",
        }
        persist_state(state)

    # A brand-new install has range metadata before it has a payload file.
    # Materialize the single staging file before it is inspected/preallocated;
    # resumed installs already have this file and are left untouched.
    partial.touch(exist_ok=True)
    if partial.stat().st_size != WEIGHT_BYTES:
        with partial.open("ab") as destination:
            destination.truncate(WEIGHT_BYTES)

    last_reported = prefix_bytes // PROGRESS_STEP_BYTES * PROGRESS_STEP_BYTES

    def completed_bytes() -> int:
        return prefix_bytes + sum(
            int(item["next"]) - int(item["start"]) for item in ranges
        )

    def download_range(item: dict[str, int]) -> None:
        nonlocal last_reported
        index = int(item["index"])
        end = int(item["end"])
        for attempt in range(1, MAX_DOWNLOAD_ATTEMPTS + 1):
            if stop_event.is_set():
                return
            with state_lock:
                position = int(item["next"])
            if position > end:
                return
            headers = {
                "Accept-Encoding": "identity",
                "Range": f"bytes={position}-{end}",
            }
            try:
                with requests.get(
                    WEIGHT_URL,
                    headers=headers,
                    stream=True,
                    timeout=(30, 90),
                    allow_redirects=True,
                ) as response:
                    if response.url.split(":", 1)[0].lower() != "https":
                        raise RuntimeError("non_https_redirect")
                    if response.status_code in {408, 425, 429} or 500 <= response.status_code <= 599:
                        raise requests.RequestException("transient_http_status")
                    if response.status_code != 206:
                        raise RuntimeError("range_not_honored")
                    expected_range = f"bytes {position}-{end}/{WEIGHT_BYTES}"
                    if response.headers.get("Content-Range") != expected_range:
                        raise RuntimeError("invalid_content_range")
                    if response.headers.get("ETag") != DOWNLOAD_ETAG:
                        raise RuntimeError("invalid_etag")
                    expected_length = end - position + 1
                    if int(response.headers.get("Content-Length", "-1")) != expected_length:
                        raise RuntimeError("invalid_content_length")

                    with partial.open("r+b", buffering=0) as destination:
                        destination.seek(position)
                        for chunk in response.iter_content(chunk_size=DOWNLOAD_CHUNK_BYTES):
                            if stop_event.is_set():
                                return
                            if not chunk:
                                continue
                            if position + len(chunk) > end + 1:
                                raise RuntimeError("range_overflow")
                            written = destination.write(chunk)
                            if written != len(chunk):
                                raise OSError("short_write")
                            position += written
                            # A persisted next offset must never get ahead of
                            # durable payload bytes. If the process or machine
                            # stops after this fsync but before the state
                            # replace, replaying the chunk is harmless.
                            os.fsync(destination.fileno())
                            with state_lock:
                                item["next"] = position
                                persist_state(state)
                                total = completed_bytes()
                                if total - last_reported >= PROGRESS_STEP_BYTES:
                                    last_reported = total
                                    print(
                                        f"model_weight_downloaded_bytes={total}",
                                        flush=True,
                                    )
                    if position != end + 1:
                        raise requests.RequestException("short_range")
                    return
            except RuntimeError:
                stop_event.set()
                raise
            except (requests.RequestException, OSError):
                if attempt == MAX_DOWNLOAD_ATTEMPTS:
                    stop_event.set()
                    raise RuntimeError(f"range_{index}_retry_exhausted")
                print(
                    f"model_weight_range_retry={index}:{attempt};next_byte={position}",
                    flush=True,
                )
                time.sleep(min(attempt, 10))

    pending_ranges = [item for item in ranges if int(item["next"]) <= int(item["end"])]
    try:
        with ThreadPoolExecutor(max_workers=RANGE_WORKERS) as executor:
            futures = [executor.submit(download_range, item) for item in pending_ranges]
            for future in as_completed(futures):
                future.result()
    except Exception:
        with state_lock:
            state["status"] = "interrupted"
            persist_state(state)
        fail("parallel model weight download failed safely; resume is available")

    with state_lock:
        if any(int(item["next"]) != int(item["end"]) + 1 for item in ranges):
            fail("model weight range map is incomplete")
        state["status"] = "verifying"
        persist_state(state)

    if sha256(partial) != WEIGHT_SHA256:
        # Keep the single staged file but distrust every byte on the next
        # idempotent run. This is self-healing without allocating a second
        # complete model copy: every range will be overwritten and rechecked.
        with state_lock:
            state["prefix_bytes"] = 0
            state["prefix_sha256"] = hashlib.sha256(b"").hexdigest()
            state["ranges"] = build_ranges(0)
            state["status"] = "hash_mismatch_retry_required"
            persist_state(state)
        fail("downloaded model weights failed integrity validation; rerun to repair")
    os.replace(partial, target)
    with state_lock:
        state["status"] = "complete"
        state["final_sha256"] = WEIGHT_SHA256
        persist_state(state)
    print(f"model_weight_verified_bytes={WEIGHT_BYTES}", flush=True)


def install_model(portable_root: Path, model_root: Path, cache_root: Path, download_root: Path) -> int:
    state_path = model_root / STATE_FILE
    revision_path = model_root / "model-revision.txt"
    entries = [path.name for path in model_root.iterdir()]
    if entries and not state_path.is_file() and not revision_path.is_file():
        fail("model target is non-empty and is not managed by Meetily")
    if revision_path.is_file() and revision_path.read_text("utf-8").strip() != REVISION:
        fail("installed model revision differs from the pinned revision")
    if state_path.is_file():
        state = json.loads(state_path.read_text("utf-8"))
        if state.get("repository") != REPOSITORY or state.get("revision") != REVISION:
            fail("download state belongs to another model or revision")
    else:
        state_path.write_text(
            json.dumps(
                {"repository": REPOSITORY, "revision": REVISION, "status": "downloading"},
                indent=2,
            ) + "\n",
            encoding="utf-8",
        )

    before_free = shutil.disk_usage(portable_root).free
    print(f"model_download_before_free_bytes={before_free}", flush=True)
    snapshot_download(
        repo_id=REPOSITORY,
        revision=REVISION,
        local_dir=str(model_root),
        cache_dir=str(cache_root),
        allow_patterns=[name for name in MODEL_FILES if name != WEIGHT_FILE],
        max_workers=4,
    )
    download_weight(download_root, model_root / WEIGHT_FILE)

    for name in MODEL_FILES:
        if not (model_root / name).is_file():
            fail("download completed without every required model file")
    for name, expected in CODE_HASHES.items():
        if sha256(model_root / name) != expected:
            fail("downloaded executable model code failed integrity validation")
    weight = model_root / WEIGHT_FILE
    if weight.stat().st_size != WEIGHT_BYTES or sha256(weight) != WEIGHT_SHA256:
        fail("downloaded model weights failed integrity validation")

    revision_path.write_text(REVISION + "\n", encoding="utf-8")
    manifest = {
        "schema": 1,
        "repository": REPOSITORY,
        "revision": REVISION,
        "weight_bytes": WEIGHT_BYTES,
        "weight_sha256": WEIGHT_SHA256,
        "files": list(MODEL_FILES),
    }
    (model_root / "meetily-model-manifest.json").write_text(
        json.dumps(manifest, indent=2) + "\n",
        encoding="utf-8",
    )
    state_path.write_text(
        json.dumps(
            {"repository": REPOSITORY, "revision": REVISION, "status": "complete"},
            indent=2,
        ) + "\n",
        encoding="utf-8",
    )
    print(f"model_installed_bytes={tree_bytes(model_root)}", flush=True)
    print(f"model_download_after_free_bytes={shutil.disk_usage(portable_root).free}", flush=True)
    return 0


def main() -> int:
    if len(sys.argv) != 3:
        fail("usage: download-moss-model.py PORTABLE_ROOT MODEL_ROOT")
    portable_root = Path(sys.argv[1]).resolve(strict=True)
    model_root = validate_portable_path(Path(sys.argv[2]).absolute(), portable_root)
    cache_root = validate_portable_path(
        portable_root / "app-data/cache/huggingface",
        portable_root,
    )
    download_root = validate_download_path(
        Path(
            os.environ.get(
                "MEETILY_MOSS_DOWNLOAD_DIR",
                str(portable_root / "app-data/temp/moss-download"),
            )
        ).absolute(),
        portable_root,
    )
    if portable_root.drive.upper() == "C:":
        fail("portable root must not resolve to the system drive")

    model_root.mkdir(parents=True, exist_ok=True)
    cache_root.mkdir(parents=True, exist_ok=True)
    download_root.mkdir(parents=True, exist_ok=True)
    model_root = model_root.resolve(strict=True)
    cache_root = cache_root.resolve(strict=True)
    download_root = download_root.resolve(strict=True)
    if not is_within(model_root, portable_root) or not is_within(cache_root, portable_root):
        fail("created path escapes the canonical portable root")

    try:
        with artifact_lock(model_root / ARTIFACT_LOCK_FILE):
            return install_model(portable_root, model_root, cache_root, download_root)
    except ArtifactLockUnavailable:
        fail("another process is already installing the pinned model")


if __name__ == "__main__":
    raise SystemExit(main())
