//! Bounded manifest detection shared by filters and repository scans.

use std::io::{BufRead, Read};

use anyhow::{Result, bail, ensure};
use git_cdc_core::manifest::{Manifest, is_manifest};

pub(crate) const MAX_MANIFEST_SIZE: u64 = 16 * 1024 * 1024;

pub(crate) fn detect(data: &[u8]) -> Option<Manifest> {
    if data.len() as u64 > MAX_MANIFEST_SIZE || !is_manifest(data) {
        return None;
    }
    Manifest::parse(data).ok()
}

/// Retain the original bytes for passthrough or replay into the chunker.
/// Ordinary content needs only a 64-byte peek; candidates stop at the cap.
pub(crate) fn read_candidate(
    reader: &mut impl BufRead,
    lfs: bool,
) -> Result<(Vec<u8>, Option<Manifest>)> {
    let mut prefix = Vec::with_capacity(64);
    (&mut *reader).take(64).read_to_end(&mut prefix)?;
    if !is_manifest(&prefix) {
        return Ok((prefix, None));
    }
    (&mut *reader)
        .take(MAX_MANIFEST_SIZE - prefix.len() as u64)
        .read_to_end(&mut prefix)?;
    if prefix.len() as u64 == MAX_MANIFEST_SIZE && !reader.fill_buf()?.is_empty() {
        return Ok((prefix, None));
    }
    let Some(manifest) = detect(&prefix) else {
        return Ok((prefix, None));
    };
    let (manifest, actual) = match manifest.extra.get("storage").map(String::as_str) {
        None => (manifest, "cdc"),
        Some("git-lfs-v1") => (crate::lfs::validate_manifest(manifest)?, "cdc-lfs"),
        Some(storage) => bail!("unsupported CDC storage backend {storage:?}"),
    };
    let expected = if lfs { "cdc-lfs" } else { "cdc" };
    ensure!(
        expected == actual,
        "manifest uses filter={actual}; cannot process it with filter={expected}; use its original backend"
    );
    Ok((prefix, Some(manifest)))
}

/// Extend `prefix` (already read from `reader`) by at most `max + 1 - len`
/// bytes. Returns the buffered bytes and whether the whole input fit within
/// `max`; otherwise the caller replays them ahead of the rest of `reader`.
pub(crate) fn read_small(
    reader: &mut impl BufRead,
    mut prefix: Vec<u8>,
    max: u64,
) -> Result<(Vec<u8>, bool)> {
    if max == 0 || prefix.len() as u64 > max {
        return Ok((prefix, false));
    }
    let remaining = max + 1 - prefix.len() as u64;
    (&mut *reader).take(remaining).read_to_end(&mut prefix)?;
    let complete = prefix.len() as u64 <= max;
    Ok((prefix, complete))
}

#[cfg(test)]
mod boundary_tests {
    use std::io::{self, BufReader, Cursor};

    use super::*;

    #[test]
    fn ordinary_content_only_needs_the_head() {
        let mut source = Cursor::new(vec![b'x'; 4096]);
        let mut reader = BufReader::with_capacity(64, &mut source);
        let (prefix, manifest) = read_candidate(&mut reader, false).unwrap();
        assert!(manifest.is_none());
        assert_eq!(prefix.len(), 64);
        assert_eq!(source.position(), 64);
    }

    #[test]
    fn candidate_reads_stop_at_the_cap_and_replay_every_byte() {
        let header = b"version git-cdc/spec/v1\n";
        for lfs in [false, true] {
            let source = Cursor::new(header).chain(io::repeat(b'x').take(MAX_MANIFEST_SIZE * 2));
            let mut reader = BufReader::with_capacity(64, source);
            let (prefix, manifest) = read_candidate(&mut reader, lfs).unwrap();
            assert!(manifest.is_none());
            assert_eq!(prefix.len() as u64, MAX_MANIFEST_SIZE);
            assert!(reader.buffer().len() <= 64);
            let (_, unread_content) = reader.get_ref().get_ref();
            assert!(unread_content.limit() >= MAX_MANIFEST_SIZE - 64);
            assert_eq!(
                io::copy(&mut prefix.as_slice().chain(reader), &mut io::sink()).unwrap(),
                header.len() as u64 + MAX_MANIFEST_SIZE * 2
            );
        }
    }

    #[test]
    fn only_complete_candidates_at_the_cap_pass_through() {
        let manifest = Manifest::new(blake3::hash(&[]), 0, vec![], Default::default());
        let mut data = manifest.encode().into_bytes();
        data.extend_from_slice(b"x-padding ");
        data.resize(MAX_MANIFEST_SIZE as usize - 1, b'x');
        data.push(b'\n');
        let (prefix, parsed) = read_candidate(&mut Cursor::new(&data), false).unwrap();
        assert!(parsed.is_some());
        assert_eq!(prefix, data);

        data.push(b'x');
        let mut reader = Cursor::new(&data);
        let (prefix, parsed) = read_candidate(&mut reader, false).unwrap();
        assert!(parsed.is_none());
        assert_eq!(prefix, data[..MAX_MANIFEST_SIZE as usize]);
        assert_eq!(reader.fill_buf().unwrap(), b"x");
    }
}
