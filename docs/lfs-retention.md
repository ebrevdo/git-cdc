# LFS retention: finding, implementation, and evidence

The first backend prototype uploaded valid LFS chunks but committed only CDC
recipes. That is sufficient for the transfer API and for an endpoint that keeps
all uploads. It is insufficient for a collector that discovers live objects
only through ordinary Git LFS pointer blobs. Retention is consequently part of
publication, rather than an assumption about uploaded objects lasting forever.

## Primary-source findings

Sources inspected on October 24, 2026:

- [Git LFS Batch API](https://github.com/git-lfs/git-lfs/blob/main/docs/api/batch.md)
  describes object transfer and verification. Its optional `ref` field is for
  authorization; it is not a portable retention transaction or object-pinning API.
- [GitHub removal documentation](https://docs.github.com/en/repositories/working-with-files/managing-large-files/removing-files-from-git-large-file-storage)
  says removed LFS objects remain in remote storage and describes deletion or
  support intervention to purge them. This supports the expectation of retained
  uploads on GitHub; no live GitHub upload/cleanup test was performed here.
- [GitLab cleanup documentation](https://docs.gitlab.com/administration/raketasks/cleanup/)
  documents removal of orphan references followed by collection of unreferenced
  LFS files. Upload success alone does not establish permanent retention.
- GitLab's [OrphanLfsFileReferences implementation](https://gitlab.com/gitlab-org/gitlab/-/blob/master/lib/gitlab/cleanup/orphan_lfs_file_references.rb)
  starts with the project's associated LFS IDs and subtracts pointers found in
  the Git repositories. The [Gitaly blob client](https://gitlab.com/gitlab-org/gitlab/-/blob/master/lib/gitlab/gitaly_client/blob_service.rb)
  supplies `--all` when requesting those pointers. Thus ordinary pointers
  reachable through an internal Git ref participate in this discovery model.

These are statements about the documented API and inspected cleanup path, not
certification of every hosted instance or every administrator's retention policy.

## Implemented storage roots

After each batch's required chunks have uploaded successfully, the backend
prepares a Git commit containing ordinary SHA-256 LFS pointer files. It publishes that
commit under `refs/notes/git-cdc-lfs/<commit-id>` in the same Git repository.
Only then does the pre-push hook allow Git to publish the user's ref updates.
There is no additional server, repository, or tracked sidecar. User history is unchanged.

Previous remote retention roots become ancestors of the new root. Parent lists
are reduced to their independent tips using Git's graph operations. Git
fast-import creates the standard pointer blobs, tree and commit in one process;
the user's index is never opened for writing. Sorted parents and a unique
publication ID give each batch a fresh commit. Immutable ref names
avoid a shared catalogue's lost-update problem. Concurrent publishers retain
both roots; subsequent roots join their ancestry.

A fresh root is important for mirrors. Git computes mirror updates before
pre-push, and deletion-only mirror operations can provide no pushed tips to the
hook. The fresh root is outside the already-negotiated deletion list, while its
ancestry keeps the older pointer roots reachable. This protects chunks even
when an ordinary clone mirrors refs without having fetched the retention refs.

A user's pre-push veto runs before storage preparation. The internal storage
push uses `--no-verify` to avoid recursively executing the same client gate;
server-side hooks remain active. A server rejection blocks the outer user push.
A client creation race succeeds only if the remote contains the identical root.

## Executed tests

`crates/cli/tests/lfs.rs` exercises the following against ordinary Git and the
unmodified Git LFS client, with the unmodified reference HTTP server and the
standard standalone-file transfer adapter:

- Stock `git lfs ls-files --all --long` sees every required CDC chunk through
  the standard pointer roots, including versions whose working files were deleted.
- A collector simulation based on that stock discovery removes a genuinely
  uploaded orphan, keeps every rooted chunk, and permits byte-exact fresh-clone
  recovery of current and historical versions after deleting the author's cache.
- A bare Git server's pre-receive rejection of retention refs prevents user refs
  from being published, proving that internal transfers do not bypass server hooks.
- An ordinary clone can perform a deletion-only mirror push without losing
  storage roots or the ability to restore content.
- Independent clones publish overlapping content concurrently, handle Git's
  normal competing branch-update rejection, rebase and retry, and reconstruct
  the merged result from a fresh clone. Identical content does not duplicate
  stored LFS bytes.
- Dry runs leave user branches and tags unchanged while preparing storage.
- Cold pushes to an up-to-date destination transfer no object content. An HTTP
  regression with two independent reference servers verifies that only missing
  destination objects are downloaded from the source.
- An 80 MiB cold mirror to an independent file remote keeps each transfer batch
  within its byte budget and evicts fetched chunks after pinning, while retaining
  a pre-existing cache object.

The collector is explicitly a local simulation using stock pointer discovery.
A live GitLab deployment and its administrative cleanup job were not run.

## Operational boundaries

- The Git server must allow this internal notes namespace. A restrictive ref
  policy causes a failed push, rather than publication without retention roots.
- The collector must consider all relevant Git refs and their ancestry. A policy
  that scans only branches/tags, excludes notes, or deletes reachable objects
  regardless of references is outside the verified retention model.
- Storage preparation can occur during `git push --dry-run`, and can remain
  after the outer push fails. It updates no user branch or tag. The pre-push
  protocol does not provide a portable dry-run indicator.
- Internal refs are absent from ordinary branch/status/diff views but appear in
  explicit all-ref inspection, such as `git log --all` or a mirror clone.
- Metadata history and refs can grow with pushes; there is no retention GC yet.
  Ordinary Git deduplicates shared pointer blobs and trees. LFS chunk identity
  and deduplication are unchanged.
- Manual deletion of the reserved namespace, client hook bypasses, or deliberate
  server-side data removal can defeat the protection. The current integration
  is a client-side publication gate, like ordinary Git LFS.
