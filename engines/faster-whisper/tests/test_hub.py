from __future__ import annotations

from types import SimpleNamespace

import pytest

from psst_engine_faster_whisper import hub
from stub_model import FakeClock

ALIASES = {"large-v3-turbo": "mobiuslabsgmbh/faster-whisper-large-v3-turbo", "tiny": "Systran/faster-whisper-tiny"}


def Bar(unit: str = "it", total: int = 0) -> SimpleNamespace:
    return SimpleNamespace(unit=unit, total=total, n=0)


def test_model_names_resolve_to_hub_repositories():
    assert hub.repo_id("large-v3-turbo", ALIASES) == "mobiuslabsgmbh/faster-whisper-large-v3-turbo"
    assert hub.repo_id("someone/faster-whisper-custom", ALIASES) == "someone/faster-whisper-custom"
    with pytest.raises(ValueError, match="large-v3-turbo, tiny"):
        hub.repo_id("huge-v9", ALIASES)


def transfer_progress(clock: FakeClock) -> tuple[hub.TransferProgress, list[tuple[int, int | None]]]:
    reports: list[tuple[int, int | None]] = []
    return hub.TransferProgress(lambda done, total: reports.append((done, total)), clock=clock, interval=0.5), reports


def test_only_the_first_byte_bar_is_reported():
    clock = FakeClock()
    progress, reports = transfer_progress(clock)
    files, sent, written = Bar(), Bar("B"), Bar("B")
    for bar in (files, sent, written):
        progress.created(bar)

    sent.n = 100
    progress.updated(sent)
    clock.advance(1)
    written.n, files.n = 50, 1
    progress.updated(written)
    progress.updated(files)
    assert reports == [(100, None)]


def test_reports_are_at_most_every_interval():
    clock = FakeClock()
    progress, reports = transfer_progress(clock)
    sent = Bar("B", total=1000)
    progress.created(sent)
    for _ in range(10):
        sent.n += 100
        progress.updated(sent)
        clock.advance(0.2)
    assert reports == [(100, 1000), (400, 1000), (700, 1000), (1000, 1000)]


def test_nothing_is_reported_after_close():
    clock = FakeClock()
    progress, reports = transfer_progress(clock)
    sent = Bar("B")
    progress.created(sent)
    progress.close()
    sent.n = 10
    progress.updated(sent)
    assert reports == []


def test_tqdm_bars_report_bytes_and_draw_nothing(capfd):
    pytest.importorskip("tqdm")
    clock = FakeClock()
    progress, reports = transfer_progress(clock)
    bar_class = progress.tqdm_class()
    with bar_class(desc="Fetching 2 files", total=2) as files:
        sent = bar_class(desc="Downloading bytes", total=0, initial=0, unit="B", unit_scale=True, name="hf")
        sent.total = 2048
        sent.update(1024)
        clock.advance(1)
        sent.update(1024)
        files.update(2)
        sent.close()
    assert reports == [(1024, 2048), (2048, 2048)]
    out, err = capfd.readouterr()
    assert (out, err) == ("", "")


def test_bars_do_not_derive_from_huggingface_hubs_tqdm():
    pytest.importorskip("huggingface_hub")
    from huggingface_hub.utils import tqdm as hf_tqdm

    progress, _ = transfer_progress(FakeClock())
    assert not issubclass(progress.tqdm_class(), hf_tqdm)


def test_model_files_are_the_ones_faster_whisper_downloads(monkeypatch):
    pytest.importorskip("faster_whisper")
    import huggingface_hub
    from faster_whisper.utils import _MODELS, download_model

    seen = {}

    def snapshot_download(repo_id, **kwargs):
        seen.update(kwargs, repo_id=repo_id)
        return "/nowhere"

    monkeypatch.setattr(huggingface_hub, "snapshot_download", snapshot_download)
    download_model("large-v3-turbo")
    assert seen["allow_patterns"] == hub.MODEL_FILES
    assert seen["repo_id"] == hub.repo_id("large-v3-turbo", _MODELS) == "mobiuslabsgmbh/faster-whisper-large-v3-turbo"
