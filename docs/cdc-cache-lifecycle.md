# CDC-side cache lifecycle

The shared CDC command layer supports indexed files from both `filter=cdc`
and `filter=cdc-lfs`. Object I/O is selected from each recipe, not from the
repository's default storage setting. Native `filter=lfs` files remain under
Git LFS's own commands.

Both adapters implement the shared lifecycle. The LFS adapter reuses verified
chunk storage and the installed Git LFS client. Remote content checks operate
on disposable sparse placeholders, with retention checked independently.

## Commands and policy

- `git cdc drop [--chunks] [--dry-run] --all | <path>...` replaces clean files
  with their exact indexed recipes. `--chunks` additionally evicts selected
  chunks with verified remote backing. Dirty, missing, and unselected files
  retain their chunk protection.
- `git cdc pull [-- <path>...]` fetches objects for selected indexed CDC files
  and restores recipe placeholders. Omitting paths selects all indexed CDC
  files. Explicitly selected missing files can also be restored. Existing
  modified content is preserved.
- `git cdc prune [--dry-run]` only evicts local cache objects. Current indexes
  and checkouts across linked worktrees, unpushed history relative to `origin`,
  and stash snapshots are protected. Objects without known recipe metadata
  are conservatively retained. Remote garbage collection is still the
  separate `gc` command.
- `git config cdc.hydrate false` explicitly leaves recipes on CDC-filtered
  checkout, including with a warm cache. It affects both the long-running and
  one-shot filters. Explicit `git cdc pull` is independent of this setting.
  Unset it or set it to `true` to restore automatic hydration. Hydration errors
  are still errors when automatic hydration is enabled.

Use `drop --chunks` to reclaim a selected current file's working copy and
cache. Normal `prune` intentionally protects the current index/checkout, even
if a file currently contains a recipe.

## Shared implementation

`crates/cli/src/lifecycle.rs` owns path selection, indexed recipe snapshots,
worktree planning, bounded reconstruction, atomic file replacement, and local
cache policy. NUL-delimited Git records preserve embedded newlines in paths.
Index blob IDs and modes are preserved when refreshing stat information;
restoration does not stage newly chunked content. Worktree writes are preceded
by index/content rechecks. Pruning checks its reference/index/worktree snapshot
again before applying its deletion plan.

`crates/cli/src/lifecycle/backend.rs` defines `CacheBackend` and typed
`ObjectId` values. The legacy adapter reuses the existing transport operations
in `sync.rs`; `lfs/cache.rs` implements LFS cache operations, and
`lfs/context.rs` isolates the stock-client verification workaround.

`remote_presence` is read-only. `Present` means safe remote backing for local
eviction, including retention where required. Missing/unknown results never
authorize deletion. A transport-level failure aborts real eviction; a dry run
can report that such chunks would be kept. Verification never calls the
mutating publication path. The stock prune check runs in an empty private
repository against exactly the candidates, with unreachable verification
enabled; its success is combined with the retention result.

Backend adapters own cache-specific locking and object validation. The LFS
adapter uses the existing SHA-256 cache and leases. Cache reads never download;
only explicit fetching populates the cache. The core
`ChunkStore` trait, recipe format, and server protocols are unchanged.

## Verification

`crates/cli/tests/lifecycle.rs` exercises shared behavior through the working
legacy adapter: staged snapshots, file modes, newline paths, selective
restoration, pointer-only checkout, cache-only pruning, stashes, unpushed
history, linked worktrees, and preservation of remote objects.

The `lifecycle_` tests in `crates/cli/tests/lfs.rs` are success contracts for
LFS drop/restoration, selected paths, dry runs, pruning and historical
recovery, and mixed-backend repositories. They use the unmodified reference
HTTP server, including negative checks for missing objects and retention,
unpushed stock pointers, multiple verification batches, dry-run isolation and
read-remote selection. File-endpoint and custom standalone-agent regressions
cover confirmation; file-endpoint tests also cover restoration.

`file://` LFS endpoints and custom standalone transfer agents require interactive
confirmation before actual eviction. The default answer is No; noninteractive
calls abort before changing working files or evicting chunks. Ordinary HTTP
transfers use the normal stock-client checks. Dry runs do not prompt.

See [the LFS implementation research](lfs-cache-implementation.md) for the
adapter design, upstream references, and the reproduced standalone-transfer
verification issue.
