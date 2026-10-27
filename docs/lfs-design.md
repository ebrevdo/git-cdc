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
attributes or the current index. Discovery at the actual push URL requests only
`refs/heads/*`, `refs/tags/*`, and `refs/notes/git-cdc-lfs/*`, avoiding unrelated
provider-managed ref lists. It excludes reachable history only for locally
available `refs/heads/` and `refs/tags/` tips, plus Git's supplied old object IDs
for the refs being updated.
Provider-managed review refs can import commits from forks without publishing
their chunks here, so they do not establish coverage. Retention roots are
processed separately and do not establish recipe coverage either.
A missing remote tip is not fetched for this optimization;
its history is conservatively included. Remote-tracking refs are never used as
evidence about the destination. Tag targets and commits no longer on a branch
are included when new to that destination. Earlier successful hook executions
establish availability and retention for already-published recipes.
Blobs are inspected through bounded reads. An oversized recognizable recipe
fails publication. Recipe scanning collects IDs, sizes and chunk hashes without
reassembling historical files. Whole-file verification remains in clean/checkout.
Full scans, used for an empty destination or `cdc.lfs.fullPush=true`, can reuse
the Git common directory's `cdc/lfs/history-v2.json`. It caches the last scan's tips
and validated chunk IDs, lengths and BLAKE3 hashes. Its version and checksum
detect stale formats and incomplete or damaged writes. Reuse requires every
cached tip to be an ancestor of a supplied tip. One `rev-list` checks all tips
through file-backed stdin; Git then walks only the added objects.
Missing objects, unrelated histories and invalid cache files cause a
full scan. Cache reads and writes are disabled while repository grafts or
`GIT_GRAFT_FILE` can alter ancestry. Version 2 invalidates earlier caches that
could contain incomplete grafted history. Writes use atomic replacement, so
concurrent pushes may lose a cache optimization without losing required metadata.
The cache stores no remote availability result. Incremental scans walk only the
destination's object difference without reading or updating this full-history
snapshot. All chunks from newly discovered recipes are negotiated, including
chunks shared with older recipes. Full-check mode negotiates every chunk in
the pushed history, for repair after bypasses or storage changes. On an
up-to-date push it checks locally available destination refs and managed roots.
The transfer session negotiates pointer metadata through the installed Git LFS
client before fetching absent content. Ref deletion requires no chunk transfer.
The hook also runs the stock LFS pre-push handler, using its ordinary cache for normal LFS
files. Both operate before Git publishes user refs. User pre-push vetoes execute
before storage transfer. A subsequent storage-only push publishes standard
pointer roots under `refs/notes/git-cdc-lfs/`; it bypasses recursive client
pre-push execution while preserving server-side hooks and ref policies.

Warm transfers use groups of at most 512 objects, bounding open chunk leases
before acquisition. Each group uses one Git LFS process; the client batches
server requests internally. Under lower descriptor limits, a warm group can
retry in 128-object batches. Cold transfers use at most 128 objects per batch.
A private pointer-only worktree lets the stock client negotiate using
IDs and sizes even with no cached content. The recovery budget is 64 MiB of missing local content by
default (`cdc.lfs.transferBatchBytes`), with oversized chunks handled
individually. HTTP batches exceeding that budget try
metadata negotiation first; if all objects already exist remotely, no recovery
or smaller batch is needed. Failed cold batches are split until missing objects
can be recovered and retried within the budget; errors are never
parsed from translated diagnostic text. Local-file transfers use the installed
`git lfs standalone-file` adapter's JSON protocol so it can check the destination
before requiring a source file. Explicit custom standalone adapters retain the
stock transfer path. File uploads first probe with an absent source path and
verify cached bytes only when the server needs an upload. HTTP/custom uploads
still verify locally cached candidates before invoking the stock client, whose
CLI does not expose the missing-object set. Incremental pushes restrict this
work to chunks from new recipes.

Retention roots are created with Git fast-import, using no staging index.
Preparation exports commit IDs without writing refs or running reference hooks;
actual publication runs the user's hooks after releasing object leases. Roots
have sorted parents, fixed identities/dates, and a unique publication ID to keep
each root outside a mirror's negotiated updates. Each new root retains previous
remote roots via Git ancestry. Existing roots are discovered once per push;
later checkpoints extend the last successful publication. The ancestry frontier
is reduced in bounded argument batches;
missing metadata is fetched without changing FETCH_HEAD or user refs. A fresh
immutable name prevents a mirror push's already-negotiated deletions from
removing the new root, even when Git provides no deletion updates to pre-push.
When there are no new chunks, one `rev-list` checks whether every remote
retention root is reachable from some local retention ref. If so, publication
is skipped: a mirror preserves the pointers through those local descendants,
even if it deletes individual remote ancestor refs. Uncovered roots or missing
objects require a fresh descendant. Its local ref covers all roots observed
during preparation, so later pushes need no guard until another publisher adds
an uncovered root. Destination discovery is shared by exclusion scanning and
retention preparation.
A failed retention publication blocks user refs. This adds metadata preparation
to dry runs, while leaving user branches and tags untouched. Transfer batches
accumulate pointers for one final root unless recovered chunks need a checkpoint
before eviction. Only chunks fetched by that transfer session are evicted, after
their checkpoint succeeds. User hooks run with no chunk leases held.

`GIT_CDC_TRACE=1` reports phase durations and object counts for history collection,
verification, transfers and retention, plus the stock LFS pre-push handler. It
also reports how many chunk records came from the history metadata cache.

## Hook installation and local cache

A global install without an existing global `core.hooksPath` creates a managed
directory and points `core.hooksPath` at it. That directory forwards Git's
documented hook names (plus git-p4 hooks) to the repository's own hooks,
preserving arguments, stdin, stdout, working directory, environment and exit
status; non-executable hooks remain inactive, and the forwarding stubs perform
no CDC storage work. Hooks already present in the active global directory are
not replaced. A custom global hook directory keeps its own semantics; only
pre-push is integrated there. Arbitrary hook names used with `git hook run`
need a repository-local hooksPath and a local install.

An existing pre-push hook in the installation directory is backed up and
chained with the same arguments and input. Chained hooks resolve beside the
installed wrapper, so repository-local hooks keep working after the repository
is relocated. Unmodified stock Git LFS pre-push hooks are handled by the
dispatcher's filtered LFS call instead of running twice. Existing
reference-transaction hooks also see the internal local ref updates, so their
policies must permit the reserved namespace.

Each pre-push invocation binds transfers and retention publication to the
destination Git supplies, including remotes with multiple push URLs; explicit
LFS endpoint settings still apply. Replacement objects are ignored because Git
publishes the original objects.

The cache is the Git common directory's `cdc/lfs/objects`, using the standard
LFS SHA-256 sharding layout. Each CDC transfer sets a relative
`lfs.storage=cdc/lfs`, so the cache stays isolated from ordinary LFS objects
and stock `git lfs prune` cannot discard unpublished CDC data whose recipes it
does not understand. The repository's ordinary LFS cache configuration is
unchanged. The standalone file transport consequently stores CDC objects under
the target repository's `cdc/lfs/objects` as well.

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

The tradeoffs are a CDC-aware client, incremental pushes that trust earlier
hook-checked publications to the destination's branches and tags,
complete-history scans for new destinations and full checks, a conservative
metadata cap, temporary disk usage, binary hunk/merge semantics, no CDC-specific
garbage collection, internal retention-ref publication, and Git's own checkout
buffering. All-ref pointer discovery now sees standard pointers through the
retention roots. Policies excluding those refs still need host-specific review.
Installation, bypasses, retention,
shallow clones and legacy-history limitations are documented in the user guide.
