# Standard LFS backend: format and integration

## Decision

Reuse the FastCDC stream chunker, the existing per-file v1 manifest, per-path
chunk parameter inheritance, and the Git filter-process protocol. Add one
opt-in storage/transport module, `crates/cli/src/lfs.rs`, and a distinct
`cdc-lfs` filter configuration so existing installations retain their behavior.
Use the installed Git LFS client for endpoint discovery, credentials, batch
negotiation, supported transfer adapters and retry behavior. The server sees
ordinary raw SHA-256-addressed LFS objects.

A per-file recipe is sufficient to reconstruct that file from a fresh clone.
No shared mutable index is committed, and neither filters nor hooks add files
to Git's index. Internal notes refs hold standard LFS pointer roots for server
retention; user commits and the main history representation remain unchanged.
The same content-addressed chunk can appear in any number of
files or commits. A local edit changes only the affected FastCDC chunks.

## Recipe encoding

The normal `version git-cdc/spec/v1` header, BLAKE3 file and chunk identities,
chunk parameters, lengths and offsets remain intact. Two reserved extensions
identify LFS storage and map the chunks' BLAKE3 IDs to standard LFS SHA-256 IDs:

```text
version git-cdc/spec/v1
chunk-avg 2097152
chunk-max 8388608
chunk-min 524288
lfs.<64 lowercase BLAKE3 hex digits> <64 lowercase SHA-256 hex digits>
oid blake3:<64 lowercase hex digits>
size <decimal bytes>
storage git-lfs-v1
chunk blake3:<64 lowercase hex digits> 0 <decimal chunk length>
```

There is one `lfs.<BLAKE3>` header per unique chunk, and one ordered `chunk`
line per occurrence. Headers retain the manifest's sorted encoding. Empty
files have no chunk references. LFS identities hash raw bytes, with no pack,
compression, machine-specific location, parent-commit dependency or mutable
catalog. The SHA mapping lives in the same blob as the file recipe, making
partial staging of paths and versions self-contained. The format uses existing
unknown-header extension points; no legacy manifest field changes meaning.
Duplicate header keys now fail parsing instead of silently overwriting.

LFS parsing requires known storage version, valid chunk parameters, contiguous
nonzero chunk lengths, a consistent total size, valid lowercase SHA-256 IDs,
no missing references, at most 65,536 chunks and at most 16 MiB of metadata.
Cache reads check SHA-256, BLAKE3 and chunk length. Reassembly checks the file
BLAKE3. Publication validates recipe metadata and cached or fetched chunks,
without reconstructing historical files. A whole-file digest manually inserted
with Git plumbing is checked during checkout, rather than publication.

## Git boundaries

`clean` consumes bytes from Git, writes verified immutable chunks to the common
cache, and returns exactly one recipe. Re-cleaning a valid recipe preserves it
byte-for-byte. It never stages a sidecar or changes a ref. `smudge` fetches any
missing chunks and returns the original file. Required-filter failures abort
Git instead of claiming a complete checkout. Filter-process output uses an
anonymous temporary file until the operation succeeds, also fixing the legacy
process filter's previous whole-file result allocation. The one-shot LFS smudge
uses the same disk-before-success discipline.

`filter=cdc-lfs diff=cdc-lfs -merge -text` gives textconv content diffs and
binary merge/patch semantics. Git retains complete authority over index stages,
conflicts, worktrees and snapshots. The chunk-parameter lookup uses the active
index, including `GIT_INDEX_FILE`, and avoids allocating large non-manifest Git
blobs. Newline-bearing paths resolve through an argv-based object lookup before
using the existing batch protocol.

The pre-push hook uses Git's supplied local object IDs, not HEAD, worktree
attributes or the current index. It walks all their ancestors without excluding
remote-tracking refs: remote Git reachability is not proof of remote LFS
availability. Tag targets and commits no longer on a branch are included.
Blobs are inspected through bounded reads. An oversized recognizable recipe
fails publication. Recipe scanning collects IDs, sizes and chunk hashes without
reassembling historical files. Whole-file verification remains in clean/checkout.
The transfer session negotiates pointer metadata through the installed Git LFS
client before fetching absent content. Ref deletion requires no chunk transfer.
The hook also runs the stock LFS pre-push handler, using its ordinary cache for normal LFS
files. Both operate before Git publishes user refs. User pre-push vetoes execute
before storage transfer. A subsequent storage-only push publishes standard
pointer roots under `refs/notes/git-cdc-lfs/`; it bypasses recursive client
pre-push execution while preserving server-side hooks and ref policies.

Transfers are limited to 128 objects and 64 MiB per batch, with oversized chunks
handled individually. A private pointer-only worktree lets the stock client
negotiate using IDs and sizes even with no cached content. Failed cold batches
are split until missing objects can be recovered and retried; errors are never
parsed from translated diagnostic text. Local-file transfers use the installed
`git lfs standalone-file` adapter's JSON protocol so it can check the destination
before requiring a source file. Explicit custom standalone adapters retain the
stock transfer path.

Retention roots are created with Git fast-import, using no staging index.
Preparation exports commit IDs without writing refs or running reference hooks;
actual publication runs the user's hooks after releasing object leases. Roots
have sorted parents, fixed identities/dates, and a unique publication ID to keep
each root outside a mirror's negotiated updates. Each new root retains previous
remote roots via Git ancestry. The ancestry frontier is reduced in bounded argument batches;
missing metadata is fetched without changing FETCH_HEAD or user refs. A fresh
immutable name prevents a mirror push's already-negotiated deletions from
removing the new root, even when Git provides no deletion updates to pre-push.
A failed retention publication blocks user refs. This adds metadata preparation
to dry runs, while leaving user branches and tags untouched. After a batch's
root succeeds, only chunks fetched by that transfer session are evicted.

## Failure and concurrency model

Chunk writes use unique same-directory temporary files, file sync, atomic
replacement and directory sync on Unix. Concurrent writers of one ID converge
on identical bytes. Interrupted writes cannot create a partially visible valid
cache object. Stale temporary files are not referenced by any recipe. A good
clean can repair a corrupt cache object using its known source bytes; reads and
publication fail on corruption. Downloaded chunks undergo the same integrity
checks as cached chunks. An unavailable endpoint or failed upload stops the
push; uploaded orphan chunks are harmless and retries deduplicate them.

There is no mutable upload-success database and no custom network API. Shared
worktrees use the Git common directory. Each clone has its own disposable cache.
Cold-cache pushes first negotiate availability at the destination. Only needed
chunks are recovered from the default download remote and, if needed, the
destination. Shared object leases protect active reads/uploads; clean and
eviction take exclusive leases. Failed transfers retain fetched content for
retry. Required objects must exist locally or at a recovery endpoint.

## Alternatives and costs

Tracked chunk sidecars would require staging extra paths during clean and would
break partial staging and index isolation. A separate repository would
complicate push/clone reachability. Internal
retention refs are used only to expose ordinary pointers to server collectors;
file recovery still uses the recipe and LFS objects without fetching those refs.
A large pack
per file would repeatedly transfer unchanged chunks. An experimental Git LFS
content extension alone does not teach the stock pointer scanner about nested
chunk dependencies. The per-file recipe plus existing pre-push integration
keeps those dependencies explicit without altering commits.

The tradeoffs are a CDC-aware client, complete-history push scans, a conservative
metadata cap, temporary disk usage, binary hunk/merge semantics, no CDC-specific
garbage collection, internal retention-ref publication, and Git's own checkout
buffering. All-ref pointer discovery now sees standard pointers through the
retention roots. Policies excluding those refs still need host-specific review.
Installation, bypasses, retention,
shallow clones and legacy-history limitations are documented in the user guide.
