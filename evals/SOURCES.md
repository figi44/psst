# Sources

Where the data in this repository comes from, and its licences.

## Common-word lists and stress sentences

`crates/psst-dict/data/common-words/en.txt` and `cs.txt` hold the 5,000 most frequent word forms
of each language, and `evals/stress/en.txt` and `cs.txt` the ordinary sentences of the
stress test. Both come from Common Voice's sentence collection, which is **CC0 1.0**
(`server/data/en/LICENSE` in the repository).

- **Repository:** [common-voice/common-voice](https://github.com/common-voice/common-voice),
  commit `3cf5dd0f81555dced52fa5a1b85f3956cdfe0c76` (2026-11-06).
- **English:** every file in `server/data/en/` except `LICENSE`.
- **Czech:** `server/data/cs/sentence-collector.txt` and `server/data/cs/wiki.cs.txt` only. Never
  `europarl-v7-cs.txt`.
- **Splits.** Each language's sentences (one per line, stripped, NFC, duplicates once) are split
  in two by the SHA-256 of their UTF-8 text: **split A** when its first byte is even, **split B**
  when it's odd. The lists use split A only, and the stress sentences split B only, so the stress
  sentences never shaped the lists.

  | Language | Sentences | Split A | Stress sentences (split B) | Stress words |
  |---|---|---|---|---|
  | en | 1,584,591 | 792,290 | 2,924 | 30,009 |
  | cs | 351,867 | 175,672 | 2,500 | 20,000 |
- **Word forms** are runs of letters joined by inner apostrophes, after case folding and NFC, with
  typographic apostrophes as `'`. Ties at equal counts go by code point order.
- **Stress sentences** are split B's sentences in the order of their SHA-256 (as hex), taken until
  the language has its word count (30,000 English, 20,000 Czech words, split on whitespace), leaving
  out any that starts with `#`. Nothing else is filtered: a sentence that happens to contain a
  dictionary term stays, and replay reports the edit.
- **Regenerating:** `python3 -I crates/psst-dict/data/derive-common-words.py <download dir>`
  downloads the files below at the commit, checks their SHA-256, and rewrites both lists and both
  stress files.

| File | SHA-256 |
|---|---|
| `server/data/en/DNGros.txt` | `d21c9183327c2a21fabef0add2f366ea4e63d19e823e3728b7ea1a2465fd9673` |
| `server/data/en/DictionaryofForeignPhrases.txt` | `8655860354c84d5efac32b7b836c6d71b033591f268f17ffca2521c51da8f9a5` |
| `server/data/en/RLarissa.txt` | `b42ce85ad34dfa5e504c02b3eb703248b973e7f94223cb70b560a50e4b76aa15` |
| `server/data/en/activate.txt` | `6d438d4d32a55bd2fba3dcd83be560e2413de150d62d6ecda653c82fb7757793` |
| `server/data/en/ajaydee.txt` | `5c76e5c6baab9e7546b62f0ca2bf7987bfbad73c2abda899627fa47c35c56295` |
| `server/data/en/bavencope.txt` | `b9a6e84d4bf2d1c5069562a92368d5ad13bd3c4fb4a08b4b0ffbd4c3de0a61a7` |
| `server/data/en/elleo.txt` | `5de742690b7e389bb1b0388466ef44dcac84f5d0ccf194244309e8bff5d06fa2` |
| `server/data/en/est31.txt` | `7278f16bba6fafb036687722c80044385332a75acbf41a16c946c1a5226203b9` |
| `server/data/en/fxvoice-cues.txt` | `263a3d6c656e293297f42f0d4f3f62e6e195e90025ee0d6e524ad7ec658dd4df` |
| `server/data/en/harvsents.txt` | `6d11a74e46a93b9463c25843d5a1f33ffcdf299a721b35b01f32dd8dc3bea657` |
| `server/data/en/james_fortune.txt` | `1803a9335ab9663d8f856b869faba61584191c87fbce5825070874ebbb5ec94f` |
| `server/data/en/jf99.txt` | `0c2bcb8ad553d1f956b36a276a1f76749f4da808175fb375b8ce91af7e23933c` |
| `server/data/en/kieran_drew.txt` | `062e6a5a30ed5475aafbe7945d41e5ee88aa5b26c52f7a9130156bbebbdd1137` |
| `server/data/en/mlennox.txt` | `8e397def2948b9701994213f4583d1a8ae974b2fe17b6a5debdeb0aa31049609` |
| `server/data/en/pro.gadget.txt` | `1484fd31ae31627e774659004aed18c9e31a7098e66d8826bd1d687935028b7f` |
| `server/data/en/reinhart1010.txt` | `6a085a2f3058b178f86bf1e099e2b4364525eaeb484134dc0895aa951edae5bf` |
| `server/data/en/sentence-collector.txt` | `31ac8e200449ebd7aee4a8c6bd16d4dafd076cf11f7c3efe1035f0283712d136` |
| `server/data/en/singleword-benchmark.txt` | `6e9ed6cf31d9e1b88aeaef9714ac89d23de04fc8f02cf58d19bd021bbcaa996a` |
| `server/data/en/thehowl.txt` | `85ad5098e647641471d0fd19e28cb874847af2466ec569ebcfa46b06257b644b` |
| `server/data/en/tlcoles.txt` | `014f7795a2b8ca5ea6877c5d970de612b7426f4ad8043e44ea6d0c9923a4bcae` |
| `server/data/en/tww.id.au.proverbs.txt` | `630df2a5d0e7c971086871135d27d6e66263635af96225c1189ca00f03406239` |
| `server/data/en/wiki.en.rerun-2021-11.txt` | `16647e64ba4124ccfeb7061e88ffb53261a16a15b7c4abcd2122b168b9f29639` |
| `server/data/en/wiki.en.txt` | `438e55e8bc91dfa6cbe098a7233702c6b177c5ae07137a01d66b9c75c9789ae5` |
| `server/data/cs/sentence-collector.txt` | `67bbf84823641f367082ab46dcafabf8e548f6fe20f50ab32dc2202c593a0b99` |
| `server/data/cs/wiki.cs.txt` | `a3c1131438133a5e30c32b9f0df7e1b37503f1b96a303be48b7971ac0c733cce` |

## The eval set

Everything in `evals/` is scripted: the sentences were written for psst, and the synthetic clips are rendered from
them. Nothing here is a recording of a real person.

### Synthetic clips (`clips/syn-*.flac`)

Rendered by `scripts/synth-evals.sh` with **Piper 1.8.0** (`piper-tts` from nixpkgs
`1d060086d60cb76a40cf37109f979d88a5bd6396`, upstream [OHF-Voice/piper1-gpl](https://github.com/OHF-Voice/piper1-gpl),
GPL-3.0-only). Piper is only the tool that renders the audio; its licence doesn't extend to the output.

Voices come from [rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices) at commit
`e7229f7d33a10e145e109b886d661984201ac3c4`. That repository is published under the MIT licence; the licence that
matters for the audio is each voice's training data, from its `MODEL_CARD`:

| Tag in clip ids | Voice | Speaker | Training data and licence | Used for |
|---|---|---|---|---|
| `lj` | [`en_US-ljspeech-high`](https://huggingface.co/rhasspy/piper-voices/tree/e7229f7d33a10e145e109b886d661984201ac3c4/en/en_US/ljspeech/high) | US English, female | [LJ Speech](https://keithito.com/LJ-Speech-Dataset/), public domain; trained from scratch | English, 1 in 3 |
| `norman` | [`en_US-norman-medium`](https://huggingface.co/rhasspy/piper-voices/tree/e7229f7d33a10e145e109b886d661984201ac3c4/en/en_US/norman/medium) | US English, male | LibriVox recordings, public domain; trained from scratch | English, 1 in 3 |
| `cori` | [`en_GB-cori-high`](https://huggingface.co/rhasspy/piper-voices/tree/e7229f7d33a10e145e109b886d661984201ac3c4/en/en_GB/cori/high) | UK English, female | LibriVox recordings, public domain; trained from scratch | English, 1 in 3 |
| `jirka-fast`, `jirka-slow` | [`cs_CZ-jirka-medium`](https://huggingface.co/rhasspy/piper-voices/tree/e7229f7d33a10e145e109b886d661984201ac3c4/cs/cs_CZ/jirka/medium) | Czech, male | [OHF-Voice/voice-datasets](https://github.com/OHF-Voice/voice-datasets), CC0; fine-tuned from the `lessac` voice (see below) | Czech, at `length_scale` 0.9 and 1.15 |

The exact files and their sha256 sums are pinned in `scripts/lib/synth_evals.py`.

**English voices were chosen** from those trained from scratch on public-domain data, so the clips carry no
attribution or share-alike terms. Voices with non-commercial data (`ryan`, `hfc_*`, `semaine`), with unclear terms
(`amy`, `alan`, `jenny_dioco`, `kusal`) or fine-tuned from `lessac` (`joe`, `lessac` itself) were left out.

**The Czech voice's licence is unsettled.** Piper has only one Czech voice. Its data
is CC0 and the model is published under MIT, but it was fine-tuned from the `lessac` checkpoint, whose data (Blizzard
2013, Voice Factory and Lessac Technologies) is under a [research-only licence](https://www.cstr.ed.ac.uk/projects/blizzard/2013/lessac_blizzard2013/license.html).
Whether that reaches audio rendered by the fine-tuned model is unclear. If it's a problem, the 33 clips in that voice
(`syn-cs-*-jirka-*`) can be dropped without touching anything else.

### What is in the set

- `sentences/en.txt` and `sentences/cs.txt`: the scripts, with dictionary terms marked `[[Term]]` (format in the
  file headers). Each entry is rendered once, with the voice picked by its number, so both splits get every voice.
- Silence and noise cases (`syn-<lang>-silence`, `-roomtone`, `-fan`, `-white`, `-hum`, `-typing`), generated with
  seeded NumPy noise. They must transcribe to nothing.
- `dictionary.toml` and `profiles.toml`: fixed snapshots, written for psst.
- `cases.jsonl`: one line per clip, in the format `psst-eval` reads (`crates/psst-eval/src/cases.rs`).

The audio is fully reproducible: Piper's noise is seeded from each clip id, so `scripts/synth-evals.sh --force`
renders byte-identical files with the same nixpkgs and voices. Checked on 2026-11-06 by rendering the whole set twice.
