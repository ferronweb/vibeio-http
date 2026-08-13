# QPACK corpus: nghttp3 encoder output

Wire bytes produced by the nghttp3 `qpack` example tool, encoding the
real-world header lists from ls-qpack `test/qifs/`. Each story is one QIF
file at one dynamic-table capacity.

## Format

A story is a JSON object with:

- `description`, `source` — provenance (commit SHAs below)
- `table_capacity` — dynamic table capacity the encoder used (set on the
  encoder stream; the decoder in tests starts at capacity 0)
- `max_blocked_streams` — encoder's blocked-stream allowance (100)
- `delay_encoder_stream` (true) — tests deliver every field section before
  any encoder stream bytes, so references to dynamic entries block and are
  unblocked as the encoder records arrive (the encoder stream is a separate
  QUIC stream and may lag the sections in practice)
- `cases` — records in the order the tool wrote them:

  - `stream_id` 0 — encoder stream bytes (fed to the decoder; no headers)
  - `stream_id` N — encoded field section for stream N (decoded, expected
    `headers` as `[[name, value], ...]` in wire order)

Sections and encoder-stream records interleave; a section may reference
entries inserted by a later encoder record, exercising blocked streams.
Stream ids run 1..N in QIF block order, so expected headers map directly.

## Source

- nghttp3: https://github.com/ngtcp2/nghttp3, commit `e4988cdb1ca9c5bfb2a591a5b132ddcadf5c739a`, MIT. Encoder run: `qpack encode test/qifs/FILE.qif OUT -s SIZE -m 100`.
- ls-qpack: https://github.com/ngtcp2/ls-qpack, commit `91567706c41c0d97ab8dc576873ecd472d7869fa`, MIT. Header lists: facebook request/response capture, NetBSD capture, and a long-codes exercise.

## Regeneration

`scripts/harvest_qpack_fixtures.sh` clones the pinned sources, builds the
tool, and rewrites this directory. Committed stories are verbatim output;
expected headers replicate the tool's QIF reader (blank-line-separated
blocks, `name\tvalue`, leading spaces of values stripped).

## Excluded sources

- quiche (cloudflare/quiche `0b1a89af`): QPACK tests are API-level; the
  fuzz corpus is an opaque length-prefixed container, not header lists.
- aioquic (aiortc/aioquic `6d36838d`): delegates QPACK to pylsqpack; tests
  are connection-level.
- ls-qpack `test/scenarios/*.sce`: shell recipes with `QIF=$(cat ...)`
  heredocs, not directly consumable.
- nghttp3/ls-qpack fuzz corpora: opaque fuzzer containers embedding
  configuration (max table size, blocked streams, encoder stream id).
