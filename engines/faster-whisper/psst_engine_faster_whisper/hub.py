"""Finding models on the Hugging Face Hub and reporting download progress."""

from __future__ import annotations

import io
import threading
import time
from collections.abc import Callable, Mapping
from typing import Any

MODEL_FILES = ["config.json", "preprocessor_config.json", "model.bin", "tokenizer.json", "vocabulary.*"]
"""The files faster-whisper 1.2.1 downloads (``allow_patterns`` in ``faster_whisper.utils.download_model``)."""

REPORT_INTERVAL_S = 0.5
"""Shortest time between two ``progress`` messages during a download."""

ReportFn = Callable[[int, "int | None"], None]
"""Receives ``(bytes, total)`` of the transfer; ``total`` is ``None`` while unknown."""


def repo_id(model: str, aliases: Mapping[str, str]) -> str:
    """The Hub repository of ``model``: a repo id as given, or a faster-whisper alias resolved.

    ``aliases`` is faster-whisper's table (``faster_whisper.utils._MODELS``), where for example
    ``large-v3-turbo`` is ``mobiuslabsgmbh/faster-whisper-large-v3-turbo``.
    """
    if "/" in model:
        return model
    try:
        return aliases[model]
    except KeyError:
        raise ValueError(f"unknown model {model!r}: expected a Hugging Face repo id or one of {', '.join(aliases)}") from None


def snapshot(
    repo: str,
    revision: str | None,
    *,
    local_files_only: bool = False,
    force_download: bool = False,
    transfer: TransferProgress | None = None,
) -> str:
    """The snapshot directory of ``repo`` at ``revision`` (default ``main``) with faster-whisper's
    files, from the Hugging Face cache in ``HF_HOME`` or downloaded into it.

    Its name is the resolved commit. Raises huggingface_hub's errors, ``LocalEntryNotFoundError``
    when ``local_files_only`` is set and the cache doesn't have it complete.
    """
    import huggingface_hub

    return huggingface_hub.snapshot_download(
        repo,
        revision=revision,
        allow_patterns=MODEL_FILES,
        local_files_only=local_files_only,
        force_download=force_download,
        tqdm_class=transfer.tqdm_class() if transfer is not None else None,
    )


class TransferProgress:
    """Turns one ``snapshot_download`` call's byte-transfer bar into progress reports.

    ``snapshot_download(tqdm_class=progress.tqdm_class())`` creates every bar from that class. In
    huggingface_hub 2.4 those are the transfer bar (bytes received, ``unit="B"``), then the
    reconstruction bar (bytes written, also ``unit="B"``) and the file-count bar; older versions
    have a single byte bar. Only the first ``unit="B"`` bar is reported, at most every
    ``interval`` seconds.

    The class derives from plain ``tqdm``, never from huggingface_hub's: that one disables itself
    when stderr isn't a terminal, which for an engine it never is. It draws nothing.
    """

    def __init__(self, report: ReportFn, *, clock: Callable[[], float] = time.monotonic, interval: float = REPORT_INTERVAL_S):
        self._report = report
        self._clock = clock
        self._interval = interval
        self._lock = threading.Lock()
        self._bar: object | None = None
        self._last: float | None = None
        self._closed = False

    def tqdm_class(self) -> type:
        """A ``tqdm`` class bound to this reporter, for ``snapshot_download(tqdm_class=...)``."""
        from tqdm.std import tqdm

        reporter = self

        class Bar(tqdm):
            monitor_interval = 0  # no monitor thread

            def __init__(self, *args: Any, **kwargs: Any) -> None:
                kwargs.pop("name", None)  # huggingface_hub's own tqdm takes it; plain tqdm doesn't
                kwargs["disable"] = False
                kwargs["file"] = io.StringIO()
                super().__init__(*args, **kwargs)
                reporter.created(self)

            def update(self, n: float | None = 1) -> bool | None:
                displayed = super().update(n)
                reporter.updated(self)
                return displayed

            def display(self, msg: str | None = None, pos: int | None = None) -> bool:
                return True

        return Bar

    def created(self, bar: Any) -> None:
        """A bar was created: the first one counting bytes is the transfer bar."""
        with self._lock:
            if self._bar is None and getattr(bar, "unit", None) == "B":
                self._bar = bar

    def updated(self, bar: Any) -> None:
        """A bar moved: report it if it's the transfer bar and the last report is old enough."""
        with self._lock:
            if bar is not self._bar or self._closed:
                return
            now = self._clock()
            if self._last is not None and now - self._last < self._interval:
                return
            self._last = now
            total = int(bar.total) if bar.total else None
            self._report(max(0, int(bar.n)), total)

    def close(self) -> None:
        """Stops reporting; bars updated afterwards are ignored."""
        with self._lock:
            self._closed = True
