# Standard Git LFS storage

The opt-in LFS backend keeps git-cdc's chunking and deduplication, but stores
the chunks as ordinary objects on your repository's existing, unmodified Git
LFS server. There is no git-cdc server, bucket, or extra repository. Every
computer that works with the repository needs Git LFS 3.x and `git-cdc`.

## Setup

Install Git LFS with your package manager, then enable the backend for every
repository on this computer:

```sh
git cdc install --lfs --global
```

This registers the `cdc-lfs` filter and a pre-push hook. If you have no global
`core.hooksPath`, it sets one to `~/.config/git-cdc/hooks`, and your
repository-local hooks keep running from there; otherwise the pre-push hook is
added to your existing hooks directory. An existing pre-push hook is
kept as `pre-push.before-cdc-lfs` and runs first; if it rejects the push,
nothing is uploaded. A repository with its own `core.hooksPath` needs a one-time
`git cdc install --lfs` inside it.

To set up a single repository instead, run `git cdc install --lfs` in it. Git
does not copy filters or hooks into a clone, so without the global install
each new clone needs:

```sh
git clone --no-checkout <url> repo && cd repo
git cdc install --lfs
git reset --hard HEAD           # first checkout, through the filter
```

## Tracking files

```sh
git cdc track --lfs '*.bin' '*.dat'
git add .gitattributes model.bin
git commit -m "add model"
git push
```

After `install --lfs`, plain `git cdc track` defaults to LFS as well. `track`
only edits `.gitattributes` and never stages anything. To convert files that
are already committed, run `git add --renormalize <paths>` and commit; this
converts the current snapshot, not older commits.

Ordinary Git LFS files (`filter=lfs`) can live alongside CDC files in the same
repository.

Git LFS uploads every object with its own request, so patterns that match many
tiny files make pushes slow. `git config cdc.smallFileMax 1m` keeps new files
up to 1 MiB as ordinary Git blobs instead; see
[small files](configuration.md#small-files). This applies to new commits
only: chunks already referenced by history still have to be uploaded.

## Choosing the LFS server

Normal Git LFS endpoint discovery applies, including `.lfsconfig`,
remote-specific LFS URLs, and Git credential helpers. A hosted LFS endpoint
usually needs no configuration. For a custom endpoint:

```sh
git config -f .lfsconfig lfs.url https://example.com/repository/info/lfs
git add .lfsconfig
git commit -m "configure LFS endpoint"
```

Never commit credentials. The server must also accept pushes to
`refs/notes/git-cdc-lfs/` (see [Pushing](#pushing)).

## Everyday use

Use `add`, `commit`, `checkout`, `switch`, `pull`, `push` and the rest of Git
normally. Checkout downloads missing chunks and verifies them before writing
the file. If content cannot be fetched (for example, while offline), the
checkout fails rather than leaving a manifest in place.

- **Diffs.** `git diff` and `git show` display reconstructed content for text
  files; binary files show their size and a fingerprint. `git show HEAD:path`
  prints the stored manifest; add `--textconv` to see the content.
- **Merges.** CDC paths merge like binary files: concurrent edits to the same
  file conflict, and you resolve the file and `git add` it.
- **Partial staging.** Staging whole files, or a version of a file you keep
  editing, works. `git add -p` does not offer hunks for CDC paths; track text
  that needs hunk staging with plain Git.
- **Manifests instead of content.** `git config cdc.hydrate false` leaves
  manifests on checkout until you run `git cdc pull`.

Chunks are cached in `.git/cdc/lfs/objects`, shared by linked worktrees and
kept apart from the ordinary LFS cache so that `git lfs prune` never touches
them. To reclaim disk:

- `git cdc drop --chunks <path>…` turns selected clean files back into
  manifests and deletes their cached chunks.
- `git cdc pull [<path>…]` brings them back.
- `git cdc prune [--dry-run]` deletes cached chunks that only already-pushed
  history needs, keeping those used by checkouts, stashes, and unpushed commits.

Chunks are only deleted after the LFS server confirms it holds them. With a
`file://` LFS endpoint or a custom standalone transfer agent, Git LFS cannot
reliably confirm this, so `drop --chunks` and `prune` ask before deleting. The
default answer is No, and non-interactive runs keep everything.

## Pushing

Before Git updates any remote refs, the pre-push hook finds CDC files in the
commits you're pushing that aren't already on the destination's branches or
tags. It checks their chunks with the LFS server and uploads any that are
missing. If an upload fails, the push fails. History already on those branches
and tags was covered by earlier pushes, so a push that only changes ordinary
files transfers no chunks. Pull-request refs don't count as covered, because
their commits can come from forks with separate LFS storage.

If a push needs chunks that aren't in the local cache, for example when
mirroring history, it downloads them in batches of up to 64 MiB and removes
them afterwards. To use less temporary disk, lower the limit with
`git config cdc.lfs.transferBatchBytes 16m`.

When there are chunks to publish, the hook pushes a small internal ref under
`refs/notes/git-cdc-lfs/`. It holds ordinary LFS pointers to those chunks, so
servers that delete unreferenced LFS objects keep them. Do not delete these
refs. They appear in `git log --all` and mirror clones, but not in branch or tag lists.
After another clone has published, your next push may publish one of these
refs with no new chunks, to keep the other clone's chunks protected.

After a hook bypass, an LFS endpoint move, missing retention refs, or suspected
server data loss, opt into a full check:

```sh
git -c cdc.lfs.fullPush=true push origin main
```

This checks every CDC file in the pushed history, uploads any missing chunks and
publishes a fresh retention ref for all of them. Missing chunks must be in your
local cache or downloadable from a remote. It works even when there's nothing
new to push. Ordinary pushes never repair history that is already published.
To check every push this way, use `git config cdc.lfs.fullPush true`.

- `git push --dry-run` still prepares required chunks and retention refs,
  because Git runs pre-push hooks for dry runs. It does not update branches or
  tags.
- `GIT_CDC_TRACE=1 git push` prints timings for each phase of the hook.

## Limits

- **Every pushing computer needs the hook.** `git push --no-verify`, hosting
  APIs, or a computer without git-cdc can publish commits whose chunks were
  never uploaded. Later ordinary pushes don't notice; run a
  [full check](#pushing) to repair them.
- **Shallow clones cannot push** (except ref deletions) until
  `git fetch --unshallow` succeeds.
- **Server retention.** Chunks are protected on servers that keep LFS objects
  reachable from any Git ref; GitLab's orphan cleanup and GitHub's retention
  policy both fit this. Collectors that scan only branches and tags, or delete
  objects regardless of references, are not covered. See the
  [retention analysis](../../lfs-retention.md).
- **No server-side cleanup.** git-cdc has no garbage collection for the LFS
  server, and the internal retention refs grow with chunk publications and
  retention-preservation updates.
- **No compression.** Chunks are stored raw, not zstd-compressed as with the
  other backends.
- **File size.** A file can have at most 65,536 chunks, roughly 128 GiB at the
  default chunk size.
- **Checkout memory.** The filter uses bounded memory and temporary disk about
  the size of the file, but Git buffers filtered checkout output, so
  `git checkout` memory still grows with file size.
- **Existing history.** Commits made with another git-cdc backend stay there;
  keep that backend configured to check them out. There is no migration.

For the recipe format and implementation, see the [design](../../lfs-design.md)
and [verification results](../../lfs-verification.md).
