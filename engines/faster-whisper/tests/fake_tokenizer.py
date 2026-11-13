from __future__ import annotations

import re

# GPT-2's pre-tokenizer, with \p{L} and \p{N} spelled for `re`.
_PIECES = re.compile(r"""'s|'t|'re|'ve|'m|'ll|'d| ?[^\W\d_]+| ?\d+| ?[^\s\w]+|\s+(?!\S)|\s+""")


def fake_encode(text: str) -> list[int]:
    """One token per 4 bytes of each piece. Like the real tokenizer, a leading space can change the
    count (``" abcd"`` is two tokens, ``"abcd"`` one), non-ASCII text costs more, and appending text
    never removes tokens."""
    ids = []
    for piece in _PIECES.findall(text):
        data = piece.encode()
        ids.extend(int.from_bytes(data[i : i + 4].ljust(4, b"\0"), "little") for i in range(0, len(data), 4))
    return ids
