from __future__ import annotations

import argparse
import hashlib
import json
import tempfile
import threading
import unittest
from dataclasses import dataclass
from pathlib import Path
from unittest import mock

import moss_worker


@dataclass(frozen=True)
class Segment:
    start: float
    end: float
    speaker: str
    text: str


class CanonicalSegmentTests(unittest.TestCase):
    def test_converts_relative_seconds_to_absolute_48k_frames(self) -> None:
        result = moss_worker.canonicalize_segments(
            [Segment(0.001, 0.100, "S01", "  hello  ")],
            96_000,
            144_000,
        )
        self.assertEqual(
            result,
            [
                {
                    "start_frame": 96_048,
                    "end_frame": 100_800,
                    "speaker": "S01",
                    "text": "hello",
                }
            ],
        )

    def test_allows_cross_speaker_overlap(self) -> None:
        result = moss_worker.canonicalize_segments(
            [
                Segment(0.0, 0.5, "S01", "one"),
                Segment(0.25, 0.75, "S02", "two"),
            ],
            0,
            48_000,
        )
        self.assertEqual(len(result), 2)

    def test_rejects_same_speaker_overlap(self) -> None:
        with self.assertRaisesRegex(
            moss_worker.InvalidInferenceResult,
            "same_speaker_overlap",
        ):
            moss_worker.canonicalize_segments(
                [
                    Segment(0.0, 0.5, "S01", "one"),
                    Segment(0.25, 0.75, "S01", "two"),
                ],
                0,
                48_000,
            )

    def test_rejects_unsorted_or_out_of_window_segments(self) -> None:
        for segments in (
            [Segment(0.5, 0.75, "S01", "later"), Segment(0.0, 0.2, "S02", "earlier")],
            [Segment(0.0, 1.01, "S01", "outside")],
        ):
            with self.subTest(segments=segments):
                with self.assertRaises(moss_worker.InvalidInferenceResult):
                    moss_worker.canonicalize_segments(segments, 0, 48_000)

    def test_rejects_invalid_speaker_and_control_text(self) -> None:
        for segment in (
            Segment(0.0, 0.1, "speaker-1", "hello"),
            Segment(0.0, 0.1, "S01", "hello\nworld"),
            Segment(0.0, 0.1, "S01", "hello\x7fworld"),
        ):
            with self.subTest(segment=segment):
                with self.assertRaises(moss_worker.InvalidInferenceResult):
                    moss_worker.canonicalize_segments([segment], 0, 48_000)


class RequestContractTests(unittest.TestCase):
    def test_bundled_reviewed_source_does_not_require_portable_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            portable = Path(directory) / "portable"
            model_root = portable / "app-data/models/moss-transcribe-diarize"
            model_root.mkdir(parents=True)
            adapter = moss_worker.LocalMossAdapter(
                portable,
                model_root,
                next(iter(moss_worker.TRUSTED_MODEL_REVISIONS)),
            )
            self.assertNotIn("sources", adapter.source_root.parts)
            manifest = json.loads(
                (adapter.source_root / moss_worker.TRUSTED_SOURCE_MANIFEST).read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(manifest["files"], moss_worker.TRUSTED_SOURCE_FILES)
            for relative, expected in moss_worker.TRUSTED_SOURCE_FILES.items():
                self.assertEqual(
                    moss_worker._reviewed_text_sha256(adapter.source_root / relative),
                    expected,
                )

    def _model_adapter_fixture(
        self,
        root: Path,
        *,
        repository: str = moss_worker.TRUSTED_MODEL_REPOSITORY,
        weight: bytes = b"good",
        expected_weight: bytes = b"good",
    ) -> moss_worker.LocalMossAdapter:
        portable = root / "portable"
        source_root = portable / "sources" / "MOSS-Transcribe-Diarize"
        model_root = portable / "app-data" / "models" / "moss-transcribe-diarize"
        source_root.mkdir(parents=True)
        model_root.mkdir(parents=True)
        for name in moss_worker.TRUSTED_MODEL_FILES:
            (model_root / name).write_bytes(b"")
        (model_root / moss_worker.TRUSTED_MODEL_WEIGHT_FILE).write_bytes(weight)
        revision = next(iter(moss_worker.TRUSTED_MODEL_REVISIONS))
        (model_root / "model-revision.txt").write_text(revision + "\n", encoding="utf-8")
        (model_root / moss_worker.TRUSTED_MODEL_MANIFEST).write_text(
            json.dumps(
                {
                    "schema": 1,
                    "repository": repository,
                    "revision": revision,
                    "weight_bytes": len(expected_weight),
                    "weight_sha256": hashlib.sha256(expected_weight).hexdigest(),
                    "files": list(moss_worker.TRUSTED_MODEL_FILES),
                }
            ),
            encoding="utf-8",
        )
        return moss_worker.LocalMossAdapter(portable, model_root, revision)

    def test_prompt_accepts_unicode_and_newlines_but_rejects_controls(self) -> None:
        self.assertEqual(
            moss_worker.validate_prompt("技术评审\nMOSS\tASR"),
            "技术评审\nMOSS\tASR",
        )
        for prompt in ("", " \t ", "x\x00y", "x\x7fy"):
            with self.subTest(prompt=repr(prompt)):
                with self.assertRaises(ValueError):
                    moss_worker.validate_prompt(prompt)

    def test_missing_model_is_explicitly_not_installed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            portable = Path(directory).resolve()
            audio_root = portable / "recordings"
            audio_root.mkdir()
            args = argparse.Namespace(
                fixture=False,
                portable_root=str(portable),
                audio_root=str(audio_root),
                model_root=str(portable / "app-data/models/moss-transcribe-diarize"),
                model_revision=next(iter(moss_worker.TRUSTED_MODEL_REVISIONS)),
            )
            status, code, _revision = moss_worker.Worker(args).initialize()
            self.assertEqual(status, "model_not_installed")
            self.assertEqual(code, "model_not_installed")

    def test_model_manifest_repository_mismatch_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            adapter = self._model_adapter_fixture(
                Path(directory),
                repository="unreviewed/example",
            )
            with self.assertRaisesRegex(
                moss_worker.AdapterUnavailable,
                "model_manifest_mismatch",
            ):
                adapter._validate_pinned_artifacts()

    def test_model_weight_hash_mismatch_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            adapter = self._model_adapter_fixture(
                Path(directory),
                weight=b"evil",
                expected_weight=b"good",
            )
            with (
                mock.patch.object(moss_worker, "TRUSTED_MODEL_WEIGHT_BYTES", 4),
                mock.patch.object(
                    moss_worker,
                    "TRUSTED_MODEL_WEIGHT_SHA256",
                    hashlib.sha256(b"good").hexdigest(),
                ),
            ):
                with self.assertRaisesRegex(
                    moss_worker.AdapterUnavailable,
                    "model_weight_hash_mismatch",
                ):
                    adapter._validate_pinned_artifacts()

    def test_cancelled_job_never_emits_late_result(self) -> None:
        worker = moss_worker.Worker.__new__(moss_worker.Worker)
        cancelled = threading.Event()
        cancelled.set()
        worker.active = {"job-1": cancelled}
        worker.active_lock = threading.Lock()
        with mock.patch.object(moss_worker, "write_message") as write:
            worker._finish_job(
                "job-1",
                cancelled,
                {"type": "result", "response": {"job": "job-1"}},
            )
        write.assert_not_called()
        self.assertEqual(worker.active, {})


if __name__ == "__main__":
    unittest.main()
