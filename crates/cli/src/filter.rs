//! The clean/smudge filters: one-shot commands and the long-running
//! filter-process protocol.

use std::io::{BufReader, BufWriter, Read, Seek, Write};

use anyhow::{Result, bail};
use git_cdc_core::chunker::Chunker;
use git_cdc_core::manifest::Manifest;
use git_cdc_core::store::{ChunkStore, DiskStore};

use crate::git::{ChunkParamsFor, chunk_params, local_store, small_file_max};
use crate::manifest::{read_candidate, read_small};

/// Clean: file content in, manifest out. Shared by the one-shot filter and
/// the filter-process loop.
///
/// Valid recipes for this backend pass through unchanged so re-cleaning a
/// pointer-only checkout never chunks the manifest text itself.
fn clean_stream(
    reader: impl Read,
    mut writer: impl Write,
    store: &DiskStore,
    params: &mut ChunkParamsFor,
    path: Option<&str>,
    chunker: &mut Chunker,
) -> Result<()> {
    let mut reader = BufReader::with_capacity(64, reader);
    let (prefix, manifest) = read_candidate(&mut reader, false)?;
    if manifest.is_some() {
        writer.write_all(&prefix)?;
        return Ok(());
    }
    // Small files go into Git unchanged (smudge passes non-recipes through).
    let (prefix, complete) = read_small(&mut reader, prefix, params.small_file_max())?;
    if complete && params.stores_plain(path, prefix.len() as u64) {
        writer.write_all(&prefix)?;
        return Ok(());
    }
    let params = params.get(path);
    let input = prefix.as_slice().chain(reader);
    let (chunks, oid, size) = chunker.chunk(input, params, |c, bytes| {
        // The chunker already computed this hash from these bytes.
        if store.has(&c.hash) {
            Ok(())
        } else {
            store.put(&c.hash, bytes)
        }
    })?;
    writer.write_all(Manifest::new(oid, size, chunks, params).encode().as_bytes())?;
    Ok(())
}

/// Smudge: manifest in, file content out (shared like `clean_stream`).
/// Non-manifest input streams through unchanged, including files committed
/// before tracking.
fn smudge_stream(reader: impl Read, mut writer: impl Write, store: &DiskStore) -> Result<()> {
    let mut reader = BufReader::with_capacity(64, reader);
    let (prefix, manifest) = read_candidate(&mut reader, false)?;
    let Some(m) = manifest else {
        writer.write_all(&prefix)?;
        std::io::copy(&mut reader, &mut writer)?;
        return Ok(());
    };

    if m.chunks.iter().any(|c| !store.has(&c.hash)) {
        // Fresh clone / chunks not fetched yet: write the manifest through
        // so checkout succeeds, and tell the user how to materialize.
        writer.write_all(&prefix)?;
        eprintln!("git-cdc: chunks not in local store; run `git cdc pull` to fetch file content");
        return Ok(());
    }

    let mut hasher = blake3::Hasher::new();
    for c in &m.chunks {
        let data = store.get(&c.hash)?; // hard-errors on corrupt chunk
        hasher.update(&data);
        writer.write_all(&data)?;
    }
    if hasher.finalize() != m.oid {
        bail!("reassembled file does not match manifest oid — refusing to emit corrupt data");
    }
    Ok(())
}

/// `path` comes from `%f` in the filter config. Installs predating that
/// pass nothing, and simply fall back to this repo's `cdc.chunk.*`.
pub fn cmd_clean(path: Option<&str>) -> Result<()> {
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    let mut params = ChunkParamsFor::new(chunk_params()?, small_file_max()?);
    clean_stream(
        stdin,
        stdout,
        &local_store()?,
        &mut params,
        path,
        &mut Chunker::default(),
    )
}

pub fn cmd_smudge() -> Result<()> {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    if !crate::git::auto_hydrate()? {
        std::io::copy(&mut stdin, &mut stdout)?;
        return Ok(());
    }
    smudge_stream(stdin, stdout, &local_store()?)
}

/// Long-running filter (gitattributes(5) filter-process protocol, v2): one
/// process per git operation instead of one per file; store and chunk
/// params are opened once.
pub fn cmd_filter_process(lfs: bool) -> Result<()> {
    use git_cdc_core::pktline::{PktReader, PktWriter, read_text, write_flush, write_text};

    let mut input = std::io::stdin().lock();
    let mut output = BufWriter::with_capacity(64 * 1024, std::io::stdout().lock());

    // Handshake: welcome + version, then capability negotiation.
    match read_text(&mut input)?.as_deref() {
        Some("git-filter-client") => {}
        other => bail!("not a git filter client (got {other:?})"),
    }
    let mut versions = Vec::new();
    while let Some(line) = read_text(&mut input)? {
        versions.push(line);
    }
    if !versions.iter().any(|v| v == "version=2") {
        bail!("no common filter protocol version (client sent {versions:?})");
    }
    write_text(&mut output, "git-filter-server")?;
    write_text(&mut output, "version=2")?;
    write_flush(&mut output)?;

    while read_text(&mut input)?.is_some() {} // client capability list
    write_text(&mut output, "capability=clean")?;
    write_text(&mut output, "capability=smudge")?;
    write_flush(&mut output)?;

    enum FilterStore {
        Disk(DiskStore),
        Lfs(crate::lfs::Store),
    }
    let store = if lfs {
        FilterStore::Lfs(crate::lfs::Store::new()?)
    } else {
        FilterStore::Disk(local_store()?)
    };
    // Per-path, so a tracked file keeps the parameters it was committed with
    // even when this clone's `cdc.chunk.*` says otherwise.
    let mut params = ChunkParamsFor::for_process(chunk_params()?, small_file_max()?);
    let hydrate = crate::git::auto_hydrate()?;
    let mut chunker = Chunker::default();
    let mut result = tempfile::spooled_tempfile(64 * 1024);
    let mut command = String::new();
    let mut pathname = String::new();

    // Per-file loop: keys, flush, content packets, flush.
    loop {
        command.clear();
        pathname.clear();
        // Git signals it's done by closing stdin between files.
        let first = match read_text(&mut input) {
            Ok(line) => line,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let mut next = first;
        while let Some(line) = next {
            match line.split_once('=') {
                Some(("command", v)) => {
                    command.clear();
                    command.push_str(v);
                }
                Some(("pathname", v)) => {
                    pathname.clear();
                    pathname.push_str(v);
                }
                _ => {} // unknown keys are fine per protocol
            }
            next = read_text(&mut input)?;
        }
        if command.is_empty() {
            bail!("filter request without a command");
        }

        // Reuse RAM for small results. Large results spill to disk, so failed
        // verification never emits partial content or holds a whole file in RAM.
        let mut content = PktReader::new(&mut input);
        if result.is_rolled() {
            result = tempfile::spooled_tempfile(64 * 1024);
        } else {
            result.set_len(0)?;
            result.rewind()?;
        }
        let outcome = match (command.as_str(), &store) {
            ("smudge", _) if !hydrate => std::io::copy(&mut content, &mut result)
                .map(|_| ())
                .map_err(Into::into),
            ("clean", FilterStore::Lfs(store)) => crate::lfs::clean(
                &mut content,
                &mut result,
                store,
                &mut params,
                Some(pathname.as_str()),
                &mut chunker,
            ),
            ("smudge", FilterStore::Lfs(store)) => {
                crate::lfs::smudge(&mut content, &mut result, store)
            }
            ("clean", FilterStore::Disk(store)) => clean_stream(
                &mut content,
                &mut result,
                store,
                &mut params,
                Some(pathname.as_str()),
                &mut chunker,
            ),
            ("smudge", FilterStore::Disk(store)) => smudge_stream(&mut content, &mut result, store),
            (other, _) => Err(anyhow::anyhow!("unsupported filter command {other:?}")),
        };
        content.drain()?;

        match outcome {
            Ok(()) => {
                result.rewind()?;
                write_text(&mut output, "status=success")?;
                output.write_all(b"0000")?;
                std::io::copy(&mut result, &mut PktWriter::new(&mut output))?;
                // Content flush, then an empty status list. Send the complete
                // response together; Git doesn't need an intermediate flush.
                output.write_all(b"00000000")?;
                output.flush()?;
            }
            Err(e) => {
                eprintln!("git-cdc: {pathname}: {e:#}");
                write_text(&mut output, "status=error")?;
                write_flush(&mut output)?;
            }
        }
    }
}
