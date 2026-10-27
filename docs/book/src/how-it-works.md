# How It Works

## Content-defined chunking

git-cdc uses **FastCDC** (the 2020 "normalized chunking" variant). A rolling
gear hash fingerprints the last few dozen bytes at every position in the
file; when the fingerprint matches a bit mask, that position becomes a chunk
boundary. Three parameters shape the result:

- **min** (default 512 KiB) — after a cut, this many bytes are skipped
  before boundary search resumes. No chunk is smaller (except a file's last
  chunk).
- **avg** (default 2 MiB) — sets the mask's bit count and therefore the cut
  probability (~1 in 2²¹ per byte for 2 MiB). Chunk sizes scatter around
  this value.
- **max** (default 8 MiB) — if no boundary appears within this many bytes,
  a cut is forced. Bounds per-chunk memory and upload size.

Because boundaries are functions of content, inserting or editing bytes
only reshapes the chunk(s) around the edit — everything after it re-aligns
to the same boundaries and keeps the same hashes. That's the property that
makes dedup work across versions.

All three are configurable per repo — see
[Configuration](configuration.md#chunk-size-tuning).

## Hashing

Every chunk is hashed with **BLAKE3**; the whole file gets its own BLAKE3
`oid`, computed in the same streaming pass as the chunking (no second read).
Chunks are stored and addressed purely by hash — identical content across
files, branches, or history is stored exactly once. Storage:

- locally in `.git/cdc/objects/<xx>/<yy>/<hex>` (sharded like git's own
  object store),
- remotely as flat `<prefix><hex>` keys in S3, or the same sharded layout
  on a server's disk.

Every read path re-verifies: chunk hashes on fetch and on smudge, the
whole-file oid after reassembly. A corrupt chunk can fail a checkout but
can never silently materialize wrong bytes.

At rest and on the wire, chunks travel in a small **envelope** — zstd-
compressed automatically when that saves more than ~5%, raw otherwise (so
already-compressed media pays no decompress cost). Identity is always the
uncompressed BLAKE3, so compression is invisible to manifests, dedup, and
GC; pre-compression stores keep working. Format:
`docs/spec/chunk-storage.md` in the repository.

## The filter pipeline

git-cdc plugs into git's clean/smudge filter machinery, exactly like
git-lfs — via the long-running **filter-process protocol**: git starts one
`git-cdc filter-process` per operation and streams every tracked file
through it over pkt-line framing, so a 500-file checkout costs one process,
not 500 (one-shot `clean`/`smudge` remain as the git < 2.11 fallback).

The process reuses its buffers from one file to the next, so tiny files need
almost no extra work, and memory stays bounded by the chunk size rather than
the file size. Each result is still verified in full before Git sees any of
it. Looking up a tracked file's committed chunk parameters is one quick query
per file for small operations; larger ones read the index once instead.

Small files aren't padded: a 512-byte file becomes one 512-byte chunk (one
512-byte LFS object with the LFS backend). Many tiny files still mean many
stored objects, so they cost filesystem work even though chunking them is
cheap.

**clean** (runs on `git add` for tracked paths): reads file content from
stdin, chunks it, writes each chunk into the local store, and emits the
manifest — which is what git actually commits. If the input is *already* a
manifest (a fresh-clone worktree that was never materialized), it passes
through unchanged rather than chunking the manifest text itself.

**smudge** (runs on checkout): reads a manifest from stdin and emits the
reassembled file. Three outcomes:

1. All chunks local → reassemble, verify the oid, emit the bytes.
2. Chunks missing → emit the manifest text itself and print
   ``run `git cdc pull` to fetch file content`` — checkout never fails on
   absence.
3. A chunk is corrupt, or the reassembled oid mismatches → hard error.
   Absence is safe; corruption is loud.

Non-manifest input (a file committed before tracking) streams through
untouched.

## The manifest

A small, strictly-formatted text file (the analogue of an LFS pointer):

```text
version git-cdc/spec/v1
chunk-avg 2097152
chunk-max 8388608
chunk-min 524288
oid blake3:fabf914c…
size 31457280
chunk blake3:9f2a1e… 0 2097152
chunk blake3:7c4d02… 2097152 2583032
…
```

Manifests are byte-stable (sorted headers, LF-only, deterministic
encoding), so identical content always produces an identical blob, and two
versions of a binary diff as a few changed `chunk` lines. The full grammar
lives in the [Manifest Format Spec](spec.md).

## Sync

**push** collects recipes from *all* reachable history, checks
candidate blobs by their size, fixed first line and full manifest parse, collects
the referenced chunk set, asks the remote which chunks it's missing (one
batch call, or one S3 listing), and uploads only those. The pre-push hook runs
this automatically, so manifests never reach the git remote ahead of their chunks.

Push caches validated chunk metadata and scans only new history when refs
advance. Missing or corrupt metadata triggers a complete scan. Every push
still checks which chunks its destination needs. History scans skip oversized
blobs without reading their contents, and prune shares reads across its
protection checks. Push leaves the cache file untouched when its history roots
are unchanged.

**pull** does the reverse for the *current checkout*: fetch missing chunks
into the local store, then materialize any tracked file whose worktree
still holds manifest text.

Pull batches index and attribute lookups, and fetches chunks only for files
that need materializing. Existing content and local edits are left untouched.
Verified files replace their recipes atomically, with one index refresh for
the operation. Pull does not force worktree fsyncs; drop keeps its durability
checks before evicting chunks.
SIGINT and SIGTERM release the index lock before exiting.

Branches need no special handling: the chunk store is branch-agnostic, and
push/gc walk every ref. Identical content on two branches is stored once.
