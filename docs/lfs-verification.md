# Standard LFS backend verification

Implementation based on upstream commit
`6a1f791f50295edd9f2ed827e30761d74789b5bc`.
Latest verification run: October 26, 2026, UTC.

## Environment and commands

- Linux x86-64; Git 2.47.3; Git LFS 3.6.1; Rust 1.98.1.
- The 23 lifecycle regressions also pass with Git LFS 3.8.1.
- Unmodified `git-lfs/lfs-test-server` at
  `d27ad23fddc1949ecd9a4ba6a3d074bc6a567084`.
- Lifecycle regressions use the reference HTTP server in both suites. Other
  workflows use file transport in the workspace suite and HTTP in the full
  LFS suite. Targeted file-workaround regressions run in both.
- The fault proxy only forwards HTTP traffic and interrupts TCP connections;
  it does not implement or replace the LFS API. The actual server receives
  partial upload bytes.

```sh
GIT_CDC_LFS_REFERENCE_SERVER=/path/to/lfs-test-server \
  cargo test --workspace --locked
GIT_CDC_LFS_TEST_SERVER=/path/to/lfs-test-server \
  cargo test -p git-cdc-cli --test lfs --locked -- --include-ignored --nocapture
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
git diff --check
```

The CI workflow builds the pinned reference server and runs both transport
suites, including the HTTP-only tests.

## Results

| Check | Result |
|---|---|
| Workspace regression suite | 157 passed, 0 failed; 2 HTTP-only tests ignored |
| Full LFS suite with reference HTTP server | 66 passed, 0 failed, 0 ignored |
| Lifecycle regressions on Git LFS 3.8.1 | 23 passed, 0 failed |
| Clippy, all targets, warnings denied | Passed |
| Rust formatting | Passed |
| Git whitespace checks | Passed |

These counts describe test executions, not distinct proofs: the explicit
reference-server test repeats the main history-recovery workflow. The original
backend, core and server regression tests remain included in the workspace run.

Push-bookkeeping regressions cover one combined retention root across several
128-object transfer batches, ancestry-cache reuse, corrupt-cache fallback,
unrelated histories, and missing remote content despite cached metadata. Git
Trace2 records one retention discovery and one retention push for an 80 MiB
fixture. With its local chunks removed, the next HTTP push reuses 19 chunk
records and completes one LFS client invocation without downloading content.
These are local regression measurements; the production migration's push time
has not been measured with this implementation.

### Publication regression checks, October 26, 2026

With Git 2.47.3, all eight added publication regressions pass against both Git
LFS 3.6.1 and 3.8.1. The earlier publication-only workspace run had five pending
lifecycle failures; the completed cache adapter now makes all of them pass.
The five cold-cache tests also pass with 3.6.1 against the reference HTTP server.

An earlier HTTP run with 3.8.1, before the cache adapter, had 35 passing tests,
two ignored tests and two failing cold-cache request-count assertions.
These failures concern extra downloads. A separate 2 MiB metadata-only cold-push
reproduction makes five extra object GET requests with both the previously
installed CDC binary and the fixed binary, establishing that this behavior
predates these publication fixes.

## Behavior exercised

- Initial commits, identical and partly shared files, incremental edits,
  chunk-parameter inheritance and historical restoration after file deletion.
- Index isolation, selected-path and staged-snapshot commits, later unstaged
  edits, alternate index files, meaningful diffs and explicit binary conflicts.
- Push, fresh clone, pull, checkout, switch, restore, real rebase, merge,
  cherry-pick, revert, reset, detached HEAD, linked worktrees and tag-only history.
- Missing and corrupted local/server chunks, invalid recipes and full-file
  digests, required-filter failures, anonymous result spooling and zero emitted
  file bytes after failed one-shot verification.
- Interrupted clean, unavailable remote, mid-body HTTP upload interruption,
  retry, concurrent clean, concurrent independent-clone uploads and competing
  branch updates followed by ordinary fetch/rebase/retry.
- Stock pointer discovery through internal retention refs, cleanup of an
  unreferenced uploaded object, cold-cache historical recovery afterward,
  server-side ref-policy rejection, mirror pushes and dry runs.
- Native hook forwarding for commits/checkouts, binary stdin/stdout, original
  exit status, vetoes, ignored non-executable hooks, custom hook directories,
  repeated installation and an original pre-push hook behind a local CDC wrapper.
- Independent publication to multiple push URLs, remote-specific LFS endpoints,
  original commit/blob publication despite replacement objects, stock Git LFS
  hooks during mirror pushes, and chained hook vetoes after repository relocation.
- LFS drop/pull/prune, missing remote objects and retention, exact-candidate
  verification despite unpushed stock pointers, multiple verification batches,
  tracking-remote selection, cache-only planning, and preservation of foreign or
  symlinked cache entries. File-endpoint and custom standalone-agent
  confirmation accepts yes, preserves files on no, and refuses unattended
  eviction. Agent coverage includes global and URL-specific settings, including
  a scope matched by Git LFS but not Git's URL matcher.
- SHA-256 Git repositories, non-idempotent URL rewrites, ambiguous branch/tag
  names, and cold restoration with relative LFS endpoints in Git config or
  `.lfsconfig` from the worktree, index and HEAD.
- `.lfsconfig` download-remote defaults and Git-config overrides, downloads
  without Git remotes, repository-local transfer-agent executables, and nested
  or conditional `.lfsconfig` includes expanded in the source repository.
  Each configuration is checked with ordinary restoration before cold CDC pull;
  remote-free eviction remains blocked without retention verification.
- Empty files, Unicode/newline/space-bearing filenames, leading-dash option
  boundaries and the one-shot filter fallback.

## Measured transfer and memory results

| Scenario | Result |
|---|---|
| 12 MiB deterministic file, default 512 KiB / 2 MiB / 8 MiB bounds, one bit changed | 5 original chunks; 1 new chunk; 2,170,503 uploaded payload bytes |
| Same size/data seed, test 64 KiB / 256 KiB / 1 MiB bounds, one byte changed | 48 original chunks; 1 new chunk; 306,415 uploaded payload bytes |
| 128 MiB file through `lfs-filter-process` | Clean and smudge both succeed under a 96 MiB address-space limit; the recipe's oid and size and the reconstructed bytes match the input |
| Sustained upload interruption | Cut after 8,192 of 646,690 object bytes; user ref unchanged; 1 completed new object retained; retry sent only the 13 missing objects |

HTTP upload-request counts and server object contents verify the incremental
transfer assertions. Payload figures exclude HTTP overhead and ordinary Git
metadata transfers. The upstream default chunk parameters are unchanged.

The filter's resident memory was approximately 16–23 MiB across observed runs;
this is an environment-specific measurement, not a universal bound. Recipe size
and chunk-size limits bound the implementation's payload/metadata allocations.
Git itself buffers filtered checkout output, so total Git-command memory is
not bounded independently of file size. No optimized throughput benchmark or
long-running production campaign was performed.

## Scope and remaining limits

The server retention fix uses ordinary LFS pointers reachable through all-ref
Git ancestry. [The retention evidence](lfs-retention.md) explains the inspected
GitLab cleanup path and the local collector simulation. Neither a live GitLab
installation nor live GitHub storage was exercised. A host must permit the
reserved notes refs and its collector must include them. Server-side ref
rejection is tested to fail closed before user refs are published.

Storage preparation, including internal refs, can occur during a dry run or
remain after an unsuccessful outer push. User branches/tags are unchanged by
that preparation. Internal refs can appear in explicit all-ref logs.
Retention-metadata GC is not implemented. Cache misses still scan complete
history, and every push negotiates remote chunk availability. Warm pushes
combine retention updates; cold migrations checkpoint recovered chunks before
eviction. Metadata-ref ancestry still adds push cost. Unverified host-specific
policies remain outside the retention guarantee.

CDC paths use binary hunk/merge semantics. Shallow non-deletion pushes remain
restricted. Legacy CDC history stays in its original backend. Client hook
bypasses and unconfigured publishing clients can circumvent the gate. The
[usage guide](book/src/lfs.md) documents installation and these limits.
