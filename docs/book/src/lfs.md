# Standard Git LFS storage

The opt-in LFS backend keeps FastCDC chunking and deduplication, and uses the
repository's existing, unmodified Git LFS server. It needs one Git repository,
Git LFS 3.x, and `git-cdc` on each participating computer. No chunk server,
extra repository, sidecar files, or server plugin is involved.

## Install and configure once

Build the updated client from this source tree:

```sh
cargo install --path crates/cli --locked
```

Install Git LFS using your operating system's package manager. To enable CDC
checkout and publication for existing and future clones on this computer:

```sh
git cdc install --lfs --global
```

This installs required `cdc-lfs` filters and a pre-push dispatcher. It sets a
global `core.hooksPath` only if you do not already have one. The managed global
directory forwards existing repository-local hooks, including commit, checkout,
merge, reference-transaction and pre-push hooks. Arguments, stdin, stdout,
working directory, environment and exit status are preserved; non-executable
hooks remain inactive. These forwarding stubs perform no CDC storage work.
An existing pre-push hook in the installation directory is backed up and
chained with
the same arguments and input; its veto runs before storage publication. Chained
hooks are resolved beside the installed wrapper, so repository-local hooks
continue to work after relocation. Unmodified stock Git LFS pre-push hooks are
handled by the dispatcher's filtered LFS call instead of being run twice. Hooks
already present in the active global directory are not replaced. Repositories
with their own
`core.hooksPath` need a one-time `git cdc install --lfs` in that repository so
the active pre-push hook is integrated too.

For repository-local installation instead, run `git cdc install --lfs` there.
Git does not copy executable filters or hooks from a repository being cloned.
Without the global installation, use `git clone --no-checkout`, install
locally, then `git reset --hard HEAD` for the first checkout. Each new clone
needs that initial local setup. Global installation avoids those per-clone
steps. Use it when you want this integration available across your repositories.
Existing local hooks continue through the managed global directory. A custom
global hook directory retains its existing semantics; the installer integrates
pre-push there without changing the other hooks. The managed forwarding list
covers Git's documented hook names (plus git-p4 hooks). Arbitrary custom names
used with `git hook run` require a repository-local hooksPath and local install.

Inside the repository, select the files once:

```sh
git cdc track --lfs '*.bin' '*.dat'
git add .gitattributes
git add model.bin
git commit -m 'Track model with deduplicated LFS storage'
git push origin main
```

`install --lfs` also selects LFS as the default for subsequent `git cdc track`
commands in its configuration scope. `track` edits only `.gitattributes`; it
never stages it or any file. Existing files can be converted by staging them
with `git add --renormalize <paths>` and committing the change. This converts
the current snapshot, not old commits or their storage dependencies.

Normal Git LFS endpoint discovery, `.lfsconfig`, remote-specific LFS URLs,
Git credential helpers, and supported transports apply. A standard hosted LFS
endpoint usually needs no additional configuration. With a custom endpoint,
configure it in the usual way, for example:

```sh
git config -f .lfsconfig lfs.url https://example.com/repository/info/lfs
git add .lfsconfig
git commit -m 'Configure LFS endpoint'
```

Commit credentials nowhere. Ordinary Git LFS files can coexist with CDC files;
the pre-push dispatcher handles both.

## Everyday use

Use `git add`, `status`, `diff`, `commit`, `push`, `clone`, `pull`, `checkout`,
`switch`, and `restore` normally. The filter writes only the content-addressed
cache. It never changes the index, refs, attributes, other tracked files, or
untracked user files. The staged recipe represents exactly the bytes supplied
to `git add`. Later edits remain unstaged, and alternate indexes work normally.

Checkout downloads missing chunks automatically, validates each SHA-256,
BLAKE3 and length, validates the complete file's BLAKE3, and only then returns
successful file content to Git. Errors fail the required filter instead of
silently leaving a successful pointer checkout. Correct content cannot be
reconstructed while offline if its chunks are absent.

`git diff` and commit patch views such as `git show HEAD` display the
reconstructed contents for text files.
Binary diffs display the byte count and a whole-content fingerprint. These
commands may download missing chunks. `--no-textconv` shows the stored recipe
when diagnosing a repository.
The raw object command `git show HEAD:path` also shows the stored recipe;
use `git show --textconv HEAD:path` to request the original content.

CDC-tracked paths have Git's binary merge policy. Independent paths merge as
usual. Concurrent edits to the same path cause a conflict with Git's ordinary
base/ours/theirs index stages. Resolve the actual file and `git add` it. Recipes
are never merged line by line. Rebase, cherry-pick, revert, resets, detached
HEAD and linked worktrees use these same index and filter semantics.

Partial staging of **files and snapshots** works: stage some paths, or stage a
version of a file and continue editing it before committing. Git cannot apply
textconv hunks to an underlying transformed binary recipe. Therefore
`git add -p` does not offer hunks for CDC paths. Track text requiring hunk
staging with ordinary Git. The extension does not implement a custom patch or
merge engine.

Normal Git checkout and push handle synchronization automatically. Use
`git cdc drop --chunks <path>` to replace selected clean files with their
indexed recipes and evict eligible chunks, `git cdc pull [<path>...]` to
restore them explicitly, and `git cdc prune` to reclaim remotely backed
historical cache entries. Dry runs are available for drop and prune.
`git config cdc.hydrate false` leaves recipes on checkout until explicit pull.

## Publication, recovery, and cache

Before Git publishes refs, the hook walks the complete Git ancestry of the
actual tips supplied by Git, including tag-only history and detached-HEAD
pushes. Replacement objects are ignored because Git publishes the original
objects. Each pre-push invocation binds transfers and retention publication to
the destination supplied by Git, including remotes with multiple push URLs;
explicit LFS endpoint settings still apply. It validates recipe structure and
collects chunk IDs and sizes without reconstructing historical files.
Whole-file hashes are computed when cleaning
content and checked during checkout; push is not a full-history integrity audit.
The hook negotiates through the installed Git LFS client before downloading
content. Objects already at the destination need no local copy. Missing chunks
are restored from the normal download remote, then the target if necessary.
Cached and fetched chunks are checked before upload.

Transfers use batches of at most 128 objects and 64 MiB; an object larger than
64 MiB travels alone. Each completed batch gets a retention root before chunks
downloaded by that push are evicted. Existing cache entries remain untouched.
Object leases coordinate readers, cleans and eviction across linked worktrees.
Failures block user ref publication and can leave uploaded objects or completed
retention roots. Retries negotiate again, without an 'already uploaded' ledger.

After successful transfer, it publishes an internal retention root under
`refs/notes/git-cdc-lfs/`. Its Git tree contains ordinary LFS pointers for the
chunks, so standard collectors scanning all Git refs can discover them. Each
new root keeps the previous remote roots as ancestors. Roots have immutable
content-addressed names, allowing concurrent clones to publish safely. This
also protects retention during `git push --mirror`, including deletion-only
mirror updates from a normal clone. User commits and the index are unchanged;
ordinary branch, tag, status and diff views do not show this namespace.
Commands explicitly inspecting every ref, such as `git log --all`, can show it.

The user pre-push veto runs first. The storage-only nested push bypasses only
the recursive client pre-push invocation; server-side hooks still run.
Existing reference-transaction hooks also see the internal local ref updates,
and their policies must permit the reserved namespace. If the
server refuses the metadata refs, the user ref update fails too. Storage
preparation can remain after a rejected or interrupted push. `git push --dry-run` can upload chunks and retention metadata because Git invokes
pre-push during dry runs; it does not update user branch or tag refs.

The local cache uses the Git common directory's `cdc/lfs/objects` with the
standard LFS SHA-256 sharding layout. All linked worktrees share it. Chunks are
written through unique temporary files, synced, and atomically replaced;
existing cache data is verified. LFS transport is configured with relative
`lfs.storage=cdc/lfs` for each CDC transfer. The cache is intentionally isolated
from ordinary LFS objects so stock `git lfs prune` cannot discard unpublished
CDC data whose recipes it does not understand. CDC overrides the local cache
path for its own transfers; the repository's ordinary LFS cache configuration
is unchanged. The standalone file transport consequently stores CDC objects
under the target repository's `cdc/lfs/objects` as well.

Deleting a working file or changing branches never deletes chunks. Local
`git cdc prune` protects indexes/checkouts, stashes and unpushed history;
`git cdc drop --chunks` selects current files explicitly. Eviction requires
advertised retention roots and a successful stock Git LFS remote check.
The check uses a disposable repository with an empty commit and sparse
placeholders for candidate objects, so source retention rules cannot skip
verification. No user refs or remote objects are changed by this check.
`file://` LFS endpoints and custom standalone transfer agents require a warning
and interactive confirmation before actual eviction because their stock
verification can report false success. The default is No; unattended calls
preserve the files and chunks. Ordinary HTTP transfers do not prompt.
Dry runs and restoration do not ask for confirmation. Push also removes
chunks it fetched after publishing their retention root. There is no CDC
command for LFS server garbage collection.
Full recovery needs the Git repository **and** its associated LFS objects,
just as with normal Git LFS. Ordinary clones hydrate CDC files through the CDC
filter. A mirror that includes the internal retention refs exposes ordinary
LFS pointers to `git lfs fetch --all` and standard pointer discovery. Ordinary
Git LFS commands do not reconstruct the CDC user-file recipes themselves.

The server must permit reading and writing the internal notes refs and retain LFS objects rooted
across all Git refs. GitLab's documented orphan cleanup uses that all-ref
pointer model; GitHub documents retention even after pointers are removed.
Collectors deliberately limited to branches or tags, and policies deleting
objects regardless of reachability, are outside this guarantee. The LFS API
alone does not define those policies. See [retention analysis and evidence](../../lfs-retention.md).
The namespace is managed storage metadata; do not manually remove it. Retention
metadata/refs can grow with pushes, and there is no retention-metadata GC yet.

## Limits and tradeoffs

- The hook is a client-side safety gate. `git push --no-verify`, disabling the
  hook, direct plumbing/hosting APIs, or pushing from an unconfigured client can
  bypass it. Git provides no client mechanism to prevent those bypasses on an
  unmodified remote. Install the integration on every publishing computer.
- A shallow repository cannot validate the complete published ancestry; the
  hook refuses non-deletion pushes until `git fetch --unshallow` succeeds.
- Every push scans metadata throughout reachable history for the supplied
  tips. A cold push downloads only objects the destination needs, using bounded
  batches. For HTTP/SSH, failed cold batches are split to identify missing
  content through the stock client's exit status. This can add process and
  negotiation overhead. A required object unavailable locally and from the
  recovery remotes is an error.
- Chunks use their raw bytes as LFS objects. Legacy CDC's zstd envelope is not
  used; SHA-256 identity and deduplication remain stable across clients.
- Manifests are limited to 65,536 chunks and 16 MiB. The default average chunk
  size corresponds to roughly 128 GiB per file, with data-dependent variation.
  The filter fails rather than exceeding the limit. Different chunk parameters
  change that capacity. Existing tracked paths inherit their staged parameters.
- Filter memory is bounded by the maximum chunk size and the capped recipe
  metadata; reconstructed bytes spool to temporary disk before success. This
  needs temporary disk space approximately equal to the reconstructed file.
  **Git itself buffers filtered checkout output**, so total `git checkout`
  memory can still grow with file size. Bounded total Git-command memory would
  require a different integration or changes to Git. The tests distinguish the
  bounded filter from the Git process.
- A file beginning with a valid CDC manifest is interpreted as a recipe, matching
  the existing pointer-style convention. Recipes should not be edited as data.
- Legacy history remains in its original backend. This extension does not
  migrate historical CDC objects to LFS or rewrite history. Keep that backend
  and its filter configured for old legacy commits. Old clients do not provide
  LFS recovery; collaborators must install this version.

See [the format and design](../../lfs-design.md) and
[verification results](../../lfs-verification.md) for implementation detail.
