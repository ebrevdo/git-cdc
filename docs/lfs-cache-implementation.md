# LFS cache lifecycle implementation

The LFS adapter completes the shared CDC drop, pull and local-prune commands.
It uses the existing SHA-256 cache and the installed Git LFS executable. It
adds no HTTP client, server extension, recipe format, or remote publication
operation to cache verification.

## Boundaries

`lifecycle.rs` owns indexed recipe snapshots, path selection, worktree planning,
streaming reconstruction, atomic file replacement and protection of current
indexes/checkouts, stashes and unpushed history. `lifecycle/backend.rs` keeps
legacy BLAKE3 and LFS SHA-256 IDs distinct and routes object operations.

`lfs/cache.rs` implements LFS cache metadata, inventory, cache-only reads,
explicit fetching and eviction under the existing object leases. Inventory
accepts only regular files with canonical lowercase SHA-256 names and sharding;
foreign files and symlinks are not eviction candidates. Reads verify SHA-256
and size; reconstruction additionally verifies each recipe's BLAKE3 hashes.

`lfs/context.rs` isolates stock-client operations behind a frozen download
context. Its `verify_remote(objects)` method returns only IDs from successful
verification batches. Temporary-repository details stay inside this adapter.
The backend's separate confirmation hook runs only before actual eviction.

## One read context per command

The context chooses the download remote using the stock client's documented
order: current branch remote, `remote.lfsdefault`, the sole configured remote,
then `origin`. The default includes `.lfsconfig` values accepted by the installed
Git LFS client, with ordinary Git configuration taking precedence. For a default
found only in `.lfsconfig`, the adapter consults the stock client's unsafe-key
diagnostic, matching its canonical key name rather than localized prose or a
hardcoded version boundary. The branch name comes from its full `refs/heads/` reference,
so a tag with the same name cannot change remote selection. Git expands URL
rewrites once; relative repository paths and explicit LFS endpoints are resolved
against the source repository before entering the private directory.
Verification and explicit restoration use this same selection rather than
independently choosing a push destination or assuming that `origin` is always
the read remote. An explicit LFS endpoint can restore content without a named
Git remote. Eviction still requires a Git destination to verify retention roots.

Effective LFS, remote, branch, HTTP, credential and SSH configuration is
copied into the private repository. URL rewrite rules are omitted after
resolving the selected Git URL and explicit LFS download URLs, preventing a
second rewrite. The effective `.lfsconfig` is taken from the worktree, index or
HEAD. Git expands its includes in the source repository, including conditional
and nested includes, before remote selection. Expanded values are written to
the private `.lfsconfig` without include directives; Git LFS still applies its
safe-key checks and ordinary configuration retains precedence. Relative
endpoints, aliases and transfer-agent executable paths are anchored to the
source repository; bare executable names retain their PATH lookup.
Repository selectors such as `GIT_DIR` and an
alternate index are cleared for private commands; credential/SSH/proxy
environment variables remain available. Git LFS retains responsibility for
endpoint resolution, authentication and transfer adapters. Its nonlocalized
`env` endpoint field is used only to identify verification risks,
not to infer object availability from human-readable verification output.

The temporary repository uses the source repository's Git object format, so
retention metadata works with both SHA-1 and SHA-256 Git repositories. It
contains one empty commit, without source branches, stashes or worktrees.
It is initialized on demand, reused across batches, and
removed automatically when the command finishes. This is an init, not a clone.
For retention verification, its Git object store reads the source clone's
objects through an alternate, including the shared store of a linked worktree.
New metadata stays private. Download contexts keep self-contained Git objects
for compatibility with stock LFS readers of alternate paths.

## Read-only backing verification

An object is eligible for eviction only when both checks succeed:

1. Its canonical pointer, including its size, is reachable from retention roots
   advertised by the selected Git destination. Metadata is fetched into the
   private repository without changing user refs or `FETCH_HEAD`. Local-only
   roots and remote-tracking refs are not sufficient.
2. Stock Git LFS verifies the object at the corresponding download endpoint.

All advertised retention refs are fetched in one request using stdin. A
streaming `rev-list --objects` walk filters canonical pointer paths against
the eviction candidates before inspecting blob contents and sizes. Object
inspection uses file-backed `cat-file` batches, without a round trip for each
object. The walk still traverses retention history, but unrelated pointer
blobs are never read by this check.

The second check uses a disposable cache under the private Git directory.
For each batch of at most 1,024 objects, the adapter clears that cache, creates
sparse placeholder files with exactly the candidate SHA-256 names and sizes,
and runs:

```sh
git lfs prune --dry-run --force --verify-remote \
  --verify-unreachable --when-unverified=halt
```

Prune normally verifies only objects eligible for deletion. Its successful exit
alone does not imply that retained objects were checked: even `--force` keeps
unpushed objects. The empty private history makes every candidate placeholder
unreachable and prunable, and `--verify-unreachable` requests its verification.
No source protection rule can silently exempt a candidate.

Successful batches become verified; every object in a failed batch stays
unverified. Localized output is never parsed to infer partial successes. The
placeholder cache is cleared between batches. This probe never points at the
real CDC cache or ordinary LFS cache, uploads content, or publishes roots.

Only verified IDs receive command-local eviction proofs. Eviction checks its
proof and obtains the object's exclusive lease before removing the cached
file. Shared planning still rechecks the index/worktree state before mutation.

## File endpoints and standalone agents

For now, stock prune verification is the content-availability primitive for
all transports. The known false-verification behavior for `file://` endpoints
and custom standalone transfer agents has a targeted workaround: an actual
eviction prints the risk and asks once whether to continue. The default is No.
Noninteractive invocations abort before working files or cached chunks are
changed. HTTP transfers without standalone
agent settings do not receive this confirmation. Detection conservatively
includes every nonempty global or URL-specific standalone-agent setting, even
when the endpoint uses HTTP. Git LFS accepts some URL scopes that Git's URL
matcher does not; assuming an unmatched setting is inactive could bypass the
safeguard.

Confirmation accepts the known verification risk. It does not bypass a
failed prune check or missing retention protection. Dry runs and restoration
do not prompt. File-transport detection uses the effective LFS endpoint: a local
Git repository whose `.lfsconfig` directs LFS to HTTP needs no confirmation
unless standalone-agent settings are present.

## Fetching and restoration

Cache reads never fetch, including during drop planning and dry runs. Explicit
fetching uses stock smudge in the private context, verifies downloaded SHA-256
and size, and installs the bytes through the shared cache's atomic writer and
exclusive lease. Temporary downloaded objects are removed after reading.
Reconstruction stays bounded by chunk size and writes through the shared
verified temporary-file path before replacing a working file.

The command-local relative `lfs.storage=cdc/lfs` override also reaches the file
adapter, so it uses the remote CDC cache rather than ordinary LFS storage.

## Verification

Lifecycle regressions use the unmodified `git-lfs/lfs-test-server` HTTP server,
not a replacement Batch API. Set `GIT_CDC_LFS_REFERENCE_SERVER` to its binary
for the workspace suite. `GIT_CDC_LFS_TEST_SERVER` additionally selects HTTP
for all other LFS workflow tests. CI builds the pinned reference server.

Coverage includes drop/pull/prune success, selected restoration, mixed
backends, missing remote content, missing retention, unpushed stock pointers,
multiple verification batches, read-remote selection and cache-only planning.
Regressions also cover SHA-256 Git repositories, ambiguous branch/tag names,
non-idempotent URL rewrites and relative endpoints in Git config or `.lfsconfig`
from the worktree, index and HEAD. Configuration regressions compare stock
restoration with cold CDC pull for `.lfsconfig` remote defaults, downloads
without Git remotes, relative agent executables, and nested or conditional
includes, including ordinary Git-config overrides and the installed client's
`.lfsconfig` safety policy. File-endpoint and custom standalone-agent
tests cover noninteractive refusal and explicit yes/no confirmation. File
tests also cover dry runs and cold restoration.

The approach follows the separation of inventory, protection, verification
and deletion in [Git LFS prune](https://github.com/git-lfs/git-lfs/blob/v3.6.1/commands/command_prune.go).
The [prune manual](https://github.com/git-lfs/git-lfs/blob/v3.6.1/docs/man/git-lfs-prune.adoc)
explains retained objects and verification flags. The
[git-annex LFS backend](https://git-annex.branchable.com/special_remotes/git-lfs/)
and [copy policy](https://git-annex.branchable.com/copies/) informed the
requirement to establish backing before dropping a local copy.
