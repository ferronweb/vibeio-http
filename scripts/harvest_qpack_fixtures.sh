#!/usr/bin/env bash
# Harvest a real-world QPACK fixture corpus into tests/fixtures/qpack/stories/.
#
# Sources (pinned):
#   - nghttp3 qpack example tool (independent encoder/decoder)
#   - ls-qpack test/qifs/*.qif (real-world header lists: facebook, netbsd)
#
# Committed output: the JSON stories + README.md under
# tests/fixtures/qpack/stories/. Everything else lives in target/qpack-harvest/
# and can be deleted; rerunning this script regenerates identical stories.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$ROOT/target/qpack-harvest"
OUT="$ROOT/tests/fixtures/qpack/stories"
NGHTTP3_COMMIT="e4988cdb1ca9c5bfb2a591a5b132ddcadf5c739a"
LSQPACK_COMMIT="91567706c41c0d97ab8dc576873ecd472d7869fa"

mkdir -p "$WORK" "$OUT/nghttp3"

clone_pinned() {
  local url="$1" dir="$2" commit="$3"
  if [ ! -d "$dir/.git" ]; then
    git clone --quiet "$url" "$dir"
  fi
  git -C "$dir" fetch --quiet origin "$commit"
  git -C "$dir" checkout --quiet --detach "$commit"
}

clone_pinned https://github.com/ngtcp2/nghttp3 "$WORK/nghttp3" "$NGHTTP3_COMMIT"
clone_pinned https://github.com/litespeedtech/ls-qpack "$WORK/ls-qpack" "$LSQPACK_COMMIT"

QPACK_BIN="$WORK/nghttp3/build/examples/qpack"
if [ ! -x "$QPACK_BIN" ]; then
  git -C "$WORK/nghttp3" submodule update --init lib/sfparse tests/munit >/dev/null
  cmake -S "$WORK/nghttp3" -B "$WORK/nghttp3/build" \
    -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF >/dev/null
  cmake --build "$WORK/nghttp3/build" --target qpack >/dev/null
fi

python3 - "$QPACK_BIN" "$WORK" "$OUT" "$NGHTTP3_COMMIT" "$LSQPACK_COMMIT" <<'PY'
import json, struct, subprocess, sys, pathlib

qpack_bin, work, out, nghttp3_commit, lsqpack_commit = sys.argv[1:6]
work, out = pathlib.Path(work), pathlib.Path(out)
qifs = work / "ls-qpack" / "test" / "qifs"

QIFS = [
    ("fb-req.qif",      256),
    ("fb-req.qif",     4096),
    ("fb-resp.qif",     256),
    ("fb-resp.qif",    4096),
    ("long-codes.qif",  256),
    ("long-codes.qif", 4096),
    ("netbsd.qif",      256),
    ("netbsd.qif",      512),
    ("netbsd.qif",     4096),
]

def clean_qif(raw):
    """Drop `#` comment lines (long-codes.qif opens with a comment block),
    preserving everything else byte-for-byte."""
    return b"\n".join(
        ln for ln in raw.split(b"\n") if not ln.startswith(b"#"))

def parse_qif(raw):
    """Header blocks, replicating nghttp3's qpack_encode.cc QIF reader:
    blank-line separated blocks, `name\tvalue` lines, leading spaces of
    values stripped. Names/values are opaque bytes; non-UTF-8 content is
    mapped 1:1 onto code points (latin-1), so JSON escapes are lossless."""
    blocks = []
    for block in clean_qif(raw).split(b"\n\n"):
        lines = [ln for ln in block.split(b"\n") if ln != b""]
        if not lines:
            continue
        headers = []
        for ln in lines:
            name, sep, value = ln.partition(b"\t")
            assert sep, f"no TAB in {ln!r}"
            headers.append([name.decode("latin-1"),
                            value.lstrip(b" ").decode("latin-1")])
        blocks.append(headers)
    return blocks

def parse_records(raw):
    records = []
    pos = 0
    while pos + 12 <= len(raw):
        sid, ln = struct.unpack(">QI", raw[pos:pos + 12])
        rec = {"stream_id": sid, "wire": raw[pos + 12:pos + 12 + ln].hex()}
        if sid != 0:
            rec["headers"] = None
        records.append(rec)
        pos += 12 + ln
    assert pos == len(raw), "trailing bytes in encoded output"
    return records

for name, size in QIFS:
    raw = (qifs / name).read_bytes()
    blocks = parse_qif(raw)
    cleaned = work / f"{name}.clean"
    cleaned.write_bytes(clean_qif(raw))
    encoded = work / f"{name}.{size}"
    subprocess.run(
        [qpack_bin, "encode", str(cleaned), str(encoded),
         "-s", str(size), "-m", "100"],
        check=True, capture_output=True)
    records = parse_records(encoded.read_bytes())
    sections = [r for r in records if r["stream_id"] != 0]
    assert len(sections) == len(blocks), (
        f"{name}@{size}: {len(sections)} sections vs {len(blocks)} blocks")
    for rec, block in zip(sections, blocks):
        rec["headers"] = block
    story = {
        "description": f"Real-world {name[:-4]} header lists encoded by the "
                       "nghttp3 qpack example tool",
        "source": f"nghttp3 {nghttp3_commit} examples/qpack_encode.cc; "
                  f"ls-qpack {lsqpack_commit} test/qifs/{name}",
        "table_capacity": size,
        "max_blocked_streams": 100,
        "delay_encoder_stream": True,
        "cases": [
            {"seqno": i, **rec} for i, rec in enumerate(records)
        ],
    }
    target = out / "nghttp3" / f"{name[:-4]}-{size}.json"
    target.write_text(json.dumps(story, indent=2, ensure_ascii=True) + "\n")
    print(f"{target.name}: {len(blocks)} sections, "
          f"{sum(len(r['wire']) // 2 for r in records)} wire bytes")
PY

readme="$OUT/nghttp3/README.md"
cat > "$readme" <<EOF
# QPACK corpus: nghttp3 encoder output

Wire bytes produced by the nghttp3 \`qpack\` example tool, encoding the
real-world header lists from ls-qpack \`test/qifs/\`. Each story is one QIF
file at one dynamic-table capacity.

## Format

A story is a JSON object with:

- \`description\`, \`source\` — provenance (commit SHAs below)
- \`table_capacity\` — dynamic table capacity the encoder used (set on the
  encoder stream; the decoder in tests starts at capacity 0)
- \`max_blocked_streams\` — encoder's blocked-stream allowance (100)
- \`delay_encoder_stream\` (true) — tests deliver every field section before
  any encoder stream bytes, so references to dynamic entries block and are
  unblocked as the encoder records arrive (the encoder stream is a separate
  QUIC stream and may lag the sections in practice)
- \`cases\` — records in the order the tool wrote them:

  - \`stream_id\` 0 — encoder stream bytes (fed to the decoder; no headers)
  - \`stream_id\` N — encoded field section for stream N (decoded, expected
    \`headers\` as \`[[name, value], ...]\` in wire order)

Sections and encoder-stream records interleave; a section may reference
entries inserted by a later encoder record, exercising blocked streams.
Stream ids run 1..N in QIF block order, so expected headers map directly.

## Source

- nghttp3: https://github.com/ngtcp2/nghttp3, commit \`e4988cdb1ca9c5bfb2a591a5b132ddcadf5c739a\`, MIT. Encoder run: \`qpack encode test/qifs/FILE.qif OUT -s SIZE -m 100\`.
- ls-qpack: https://github.com/ngtcp2/ls-qpack, commit \`91567706c41c0d97ab8dc576873ecd472d7869fa\`, MIT. Header lists: facebook request/response capture, NetBSD capture, and a long-codes exercise.

## Regeneration

\`scripts/harvest_qpack_fixtures.sh\` clones the pinned sources, builds the
tool, and rewrites this directory. Committed stories are verbatim output;
expected headers replicate the tool's QIF reader (blank-line-separated
blocks, \`name\\tvalue\`, leading spaces of values stripped).

## Excluded sources

- quiche (cloudflare/quiche \`0b1a89af\`): QPACK tests are API-level; the
  fuzz corpus is an opaque length-prefixed container, not header lists.
- aioquic (aiortc/aioquic \`6d36838d\`): delegates QPACK to pylsqpack; tests
  are connection-level.
- ls-qpack \`test/scenarios/*.sce\`: shell recipes with \`QIF=\$(cat ...)\`
  heredocs, not directly consumable.
- nghttp3/ls-qpack fuzz corpora: opaque fuzzer containers embedding
  configuration (max table size, blocked streams, encoder stream id).
EOF

echo "harvest complete -> $OUT/nghttp3"
