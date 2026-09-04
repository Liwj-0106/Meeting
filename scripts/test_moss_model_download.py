from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import requests


SCRIPT = Path(__file__).with_name("download-moss-model.py")
SPEC = importlib.util.spec_from_file_location("meetily_moss_download", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
download = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = download
SPEC.loader.exec_module(download)


class PartialResponse:
    status_code = 206
    url = "https://cdn.example.invalid/model"

    def __init__(self) -> None:
        self.headers = {
            "Content-Range": f"bytes 0-3/{download.WEIGHT_BYTES}",
            "Content-Length": "4",
            "ETag": download.DOWNLOAD_ETAG,
        }

    def __enter__(self) -> "PartialResponse":
        return self

    def __exit__(self, *_args: object) -> None:
        return None

    @staticmethod
    def iter_content(*, chunk_size: int):
        del chunk_size
        yield b"go"
        raise requests.ConnectionError("synthetic interruption")


class FullResponse(PartialResponse):
    def __init__(self, status_code: int) -> None:
        super().__init__()
        self.status_code = status_code

    @staticmethod
    def iter_content(*, chunk_size: int):
        del chunk_size
        yield b"good"


class DownloadSafetyTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows artifact lock contract")
    def test_artifact_lock_rejects_a_second_installer(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            lock = Path(directory) / "artifact.lock"
            with download.artifact_lock(lock):
                with self.assertRaises(download.ArtifactLockUnavailable):
                    with download.artifact_lock(lock):
                        self.fail("a second installer acquired the artifact lock")

    def test_interrupted_checkpoint_never_advances_past_payload(self) -> None:
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(download, "WEIGHT_BYTES", 4),
            mock.patch.object(download, "WEIGHT_SHA256", hashlib.sha256(b"good").hexdigest()),
            mock.patch.object(download, "RANGE_WORKERS", 1),
            mock.patch.object(download, "MAX_DOWNLOAD_ATTEMPTS", 1),
            mock.patch.object(download.requests, "get", return_value=PartialResponse()),
        ):
            root = Path(directory)
            with self.assertRaises(SystemExit):
                download.download_weight(root, root / "installed.safetensors")
            state = json.loads((root / download.RANGE_STATE_FILE).read_text("utf-8"))
            self.assertEqual(state["status"], "interrupted")
            self.assertEqual(state["ranges"][0]["next"], 2)
            self.assertEqual(
                (root / f"{download.WEIGHT_FILE}.part").read_bytes()[:2],
                b"go",
            )

    def test_final_hash_mismatch_resets_every_range_for_repair(self) -> None:
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(download, "WEIGHT_BYTES", 4),
            mock.patch.object(download, "WEIGHT_SHA256", hashlib.sha256(b"good").hexdigest()),
            mock.patch.object(download, "RANGE_WORKERS", 1),
        ):
            root = Path(directory)
            part = root / f"{download.WEIGHT_FILE}.part"
            part.write_bytes(b"evil")
            state = {
                "schema": 1,
                "repository": download.REPOSITORY,
                "revision": download.REVISION,
                "weight_file": download.WEIGHT_FILE,
                "weight_bytes": 4,
                "weight_sha256": hashlib.sha256(b"good").hexdigest(),
                "etag": download.DOWNLOAD_ETAG,
                "prefix_bytes": 0,
                "prefix_sha256": hashlib.sha256(b"").hexdigest(),
                "ranges": [{"index": 0, "start": 0, "end": 3, "next": 4}],
                "status": "verifying",
            }
            (root / download.RANGE_STATE_FILE).write_text(
                json.dumps(state),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(SystemExit, "rerun to repair"):
                download.download_weight(root, root / "installed.safetensors")
            repaired = json.loads((root / download.RANGE_STATE_FILE).read_text("utf-8"))
            self.assertEqual(repaired["status"], "hash_mismatch_retry_required")
            self.assertEqual(repaired["prefix_bytes"], 0)
            self.assertEqual(repaired["ranges"][0]["next"], 0)

    def test_transient_http_status_retries_without_resetting_the_range(self) -> None:
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(download, "WEIGHT_BYTES", 4),
            mock.patch.object(download, "WEIGHT_SHA256", hashlib.sha256(b"good").hexdigest()),
            mock.patch.object(download, "RANGE_WORKERS", 1),
            mock.patch.object(download, "MAX_DOWNLOAD_ATTEMPTS", 2),
            mock.patch.object(download.time, "sleep"),
            mock.patch.object(
                download.requests,
                "get",
                side_effect=[FullResponse(503), FullResponse(206)],
            ),
        ):
            root = Path(directory)
            target = root / "installed.safetensors"
            download.download_weight(root, target)
            self.assertEqual(target.read_bytes(), b"good")


if __name__ == "__main__":
    unittest.main()
