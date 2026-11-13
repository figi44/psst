"""faster-whisper and the Hugging Face cache, faked at the engine's ``Backend``."""

from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass, field
from types import SimpleNamespace
from typing import Any

from fake_tokenizer import fake_encode
from psst_engine_faster_whisper.model import Backend

REMOTE = {
    "mobiuslabsgmbh/faster-whisper-large-v3-turbo": "0123456789abcdef0123456789abcdef01234567",
    "Systran/faster-whisper-tiny": "d90ca5fe260221311c53c58e660288d3deb8d356",
}
ALIASES = {"large-v3-turbo": "mobiuslabsgmbh/faster-whisper-large-v3-turbo", "tiny": "Systran/faster-whisper-tiny"}


def snapshot_dir(repo: str, commit: str) -> str:
    return f"/hf/hub/models--{repo.replace('/', '--')}/snapshots/{commit}"


@dataclass
class Bar:
    """What huggingface_hub's progress bars look like to the engine."""

    unit: str = "it"
    total: int = 0
    n: int = 0


@dataclass
class FakeWhisper:
    """The Hub, the local cache, and faster-whisper's models, as the engine sees them."""

    cached: set[str] = field(default_factory=set)
    """Repositories whose snapshot is complete in the local cache."""
    broken: dict[str, Exception] = field(default_factory=dict)
    """Snapshot directories that fail to load with an error, until downloaded again by force."""
    download_error: Exception | None = None
    fail_next: Exception | None = None
    """Raised while the next transcription's segments are read, as faster-whisper does."""
    segments: list[Any] = field(default_factory=lambda: [default_segment()])
    duration_after_vad: float = 2.5
    language: str = "en"

    downloads: list[dict[str, Any]] = field(default_factory=list)
    models: list[dict[str, Any]] = field(default_factory=list)
    runs: list[tuple[str, dict[str, Any]]] = field(default_factory=list)
    """``(path, kwargs)`` of every ``transcribe`` call, ``path`` being sequential or batched."""
    warmed_up: list[Any] = field(default_factory=list)
    warm_up_error: Exception | None = None

    def backend(self) -> Backend:
        return Backend(
            whisper_model=self._whisper_model,
            batched_pipeline=lambda model: SimpleNamespace(transcribe=lambda audio, **kw: self._run("batched", audio, kw)),
            snapshot=self._snapshot,
            aliases=ALIASES,
            languages={"en", "cs", "de"},
            audio=lambda samples: samples,
            warm_up=self._warm_up,
        )

    def _warm_up(self, model: Any) -> None:
        if self.warm_up_error is not None:
            raise self.warm_up_error
        self.warmed_up.append(model)

    def _snapshot(
        self, repo: str, revision: str | None, *, local_files_only: bool = False, force_download: bool = False, transfer: Any = None
    ) -> str:
        if repo not in REMOTE:
            raise OSError(f"404: {repo} not found")
        path = snapshot_dir(repo, revision or REMOTE[repo])
        if local_files_only:
            if repo not in self.cached:
                raise FileNotFoundError(f"{repo} is not cached")
            return path
        self.downloads.append({"repo": repo, "revision": revision, "force_download": force_download})
        if self.download_error is not None:
            raise self.download_error
        if transfer is not None:
            # huggingface_hub 2.4: the file-count bar, then the transfer and reconstruction bars.
            files, sent, written = Bar(), Bar("B"), Bar("B")
            for bar in (files, sent, written):
                transfer.created(bar)
            sent.total = written.total = 1000
            for got, wrote in ((400, 300), (600, 700)):
                sent.n += got
                transfer.updated(sent)
                written.n += wrote
                transfer.updated(written)
            files.n = 1
            transfer.updated(files)
        self.cached.add(repo)
        if force_download:
            self.broken.pop(path, None)
        return path

    def _whisper_model(self, path: str, *, device: str, compute_type: str) -> Any:
        self.models.append({"path": path, "device": device, "compute_type": compute_type})
        if path in self.broken:
            raise self.broken[path]
        tokenizer = SimpleNamespace(encode=lambda text, add_special_tokens=True: SimpleNamespace(ids=fake_encode(text)))
        return SimpleNamespace(hf_tokenizer=tokenizer, transcribe=lambda audio, **kw: self._run("sequential", audio, kw))

    def _run(self, path: str, audio: Any, kwargs: dict[str, Any]) -> tuple[Iterator[Any], Any]:
        self.runs.append((path, kwargs))
        failure, self.fail_next = self.fail_next, None

        def segments() -> Iterator[Any]:
            if failure is not None:
                raise failure
            yield from self.segments

        info = SimpleNamespace(language=kwargs.get("language") or self.language, duration_after_vad=self.duration_after_vad)
        return segments(), info


def default_segment(words: bool = True) -> Any:
    return SimpleNamespace(
        text=" Push the fix to psst.",
        start=0.0,
        end=2.1,
        no_speech_prob=0.0123456,
        words=[
            SimpleNamespace(word=" Push", start=0.0, end=0.3, probability=0.98),
            SimpleNamespace(word=" the", start=0.3, end=0.45, probability=0.99),
            SimpleNamespace(word=" fix", start=0.45, end=0.8, probability=0.97),
            SimpleNamespace(word=" to", start=0.8, end=0.95, probability=0.99),
            SimpleNamespace(word=" psst.", start=0.95, end=2.1, probability=0.612345),
        ]
        if words
        else None,
    )
