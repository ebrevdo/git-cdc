use std::io::Read;

use anyhow::{Result, bail};
use fastcdc::v2020::FastCDC;

// DESIGN.md §4 default chunking parameters; override per repo with
// `cdc.chunk.{min,avg,max}` git config.
pub const MIN_SIZE: u32 = 512 * 1024;
pub const AVG_SIZE: u32 = 2 * 1024 * 1024;
pub const MAX_SIZE: u32 = 8 * 1024 * 1024;

/// The largest chunk any client may produce (fastcdc's MAXIMUM_MAX) — the
/// server sizes its request-body limit off this, not the client default.
pub const CEILING: u32 = fastcdc::v2020::MAXIMUM_MAX as u32;

/// FastCDC bounds for one chunking run. Manifests record the values used;
/// readers never need them (reassembly is by chunk hash alone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkParams {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl Default for ChunkParams {
    fn default() -> Self {
        ChunkParams {
            min: MIN_SIZE,
            avg: AVG_SIZE,
            max: MAX_SIZE,
        }
    }
}

impl ChunkParams {
    /// fastcdc only `debug_assert`s its bounds — a release build silently
    /// chunks wrong on out-of-range values, so enforce them here.
    pub fn validate(self) -> Result<Self> {
        use fastcdc::v2020::{
            AVERAGE_MAX, AVERAGE_MIN, MAXIMUM_MAX, MAXIMUM_MIN, MINIMUM_MAX, MINIMUM_MIN,
        };
        let ranges = [
            ("cdc.chunk.min", self.min as usize, MINIMUM_MIN, MINIMUM_MAX),
            ("cdc.chunk.avg", self.avg as usize, AVERAGE_MIN, AVERAGE_MAX),
            ("cdc.chunk.max", self.max as usize, MAXIMUM_MIN, MAXIMUM_MAX),
        ];
        for (key, value, lo, hi) in ranges {
            if value < lo || value > hi {
                bail!("{key} = {value} is out of range ({lo}..={hi} bytes)");
            }
        }
        if !(self.min <= self.avg && self.avg <= self.max) {
            bail!(
                "chunk sizes must satisfy min <= avg <= max, got {} / {} / {}",
                self.min,
                self.avg,
                self.max
            );
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub hash: blake3::Hash,
    pub offset: u64,
    pub length: u32,
}

/// Chunk a stream with FastCDC. `sink` is called once per chunk with its raw
/// bytes (store them, upload them — caller's choice), keeping memory bounded
/// by the maximum chunk size instead of file size. Returns the chunk list,
/// the whole-file BLAKE3 oid (computed in the same pass), and the total size.
pub fn chunk_stream<R: Read>(
    reader: R,
    params: ChunkParams,
    sink: impl FnMut(&Chunk, &[u8]) -> Result<()>,
) -> Result<(Vec<Chunk>, blake3::Hash, u64)> {
    Chunker::default().chunk(reader, params, sink)
}

/// Reusable streaming workspace. Tiny files need only a small buffer; larger
/// files grow it up to two maximum-sized windows, amortizing compaction of
/// the unconsumed tail. The sink borrows chunk bytes directly, avoiding
/// StreamCDC's allocation and copy of every chunk.
#[derive(Default)]
pub struct Chunker {
    buffer: Vec<u8>,
}

impl Chunker {
    pub fn chunk(
        &mut self,
        mut reader: impl Read,
        params: ChunkParams,
        mut sink: impl FnMut(&Chunk, &[u8]) -> Result<()>,
    ) -> Result<(Vec<Chunk>, blake3::Hash, u64)> {
        let params = params.validate()?;
        let max = params.max as usize;
        let mut chunks = Vec::new();
        let mut file_hasher = blake3::Hasher::new();
        let mut size = 0;
        let (mut start, mut end) = (0, 0);
        let mut eof = false;

        loop {
            // A full maximum-sized window (or EOF) is required to preserve
            // FastCDC boundaries regardless of how the reader splits input.
            while end - start < max && !eof {
                if end == self.buffer.len() {
                    if start == end && start > 0 {
                        start = 0;
                        end = 0;
                    } else if self.buffer.len() >= 2 * max {
                        self.buffer.copy_within(start..end, 0);
                        end -= start;
                        start = 0;
                    } else {
                        let capacity = (self.buffer.len() * 2).max(8192).min(2 * max);
                        self.buffer.reserve_exact(capacity - self.buffer.len());
                        self.buffer.resize(capacity, 0);
                    }
                }
                let limit = self.buffer.len().min(start + max);
                match reader.read(&mut self.buffer[end..limit]) {
                    Ok(0) => eof = true,
                    Ok(n) => end += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if start == end {
                break;
            }
            let length = FastCDC::new(
                &self.buffer[start..end],
                params.min as usize,
                params.avg as usize,
                max,
            )
            .next()
            .unwrap()
            .length;
            let bytes = &self.buffer[start..start + length];
            let chunk = Chunk {
                hash: blake3::hash(bytes),
                offset: size,
                length: length as u32,
            };
            sink(&chunk, bytes)?;
            chunks.push(chunk);
            if eof && size == 0 && start + length == end {
                // The overwhelmingly common tiny-file case: the chunk hash
                // is already the whole-file hash.
                return Ok((chunks, chunk.hash, length as u64));
            }
            file_hasher.update(bytes);
            size += length as u64;
            start += length;
        }
        Ok((chunks, file_hasher.finalize(), size))
    }
}

/// Test/bench helpers. Compiled unconditionally (not cfg(test)) so other
/// crates' test binaries and benches can share them.
pub mod test_util {
    /// Deterministic pseudo-random bytes without a rand dependency.
    pub fn test_data(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::test_data;
    use super::*;

    fn chunk_all(data: &[u8]) -> (Vec<Chunk>, blake3::Hash, u64) {
        chunk_stream(data, ChunkParams::default(), |_, _| Ok(())).unwrap()
    }

    #[test]
    fn bounds_and_sizes_respected() {
        let data = test_data(20 * 1024 * 1024, 42);
        let (chunks, oid, size) = chunk_all(&data);

        assert_eq!(size, data.len() as u64);
        assert_eq!(oid, blake3::hash(&data));
        assert_eq!(
            chunks.iter().map(|c| c.length as u64).sum::<u64>(),
            data.len() as u64
        );
        // All but the last chunk respect the min bound; all respect max.
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.length >= MIN_SIZE, "chunk below min: {}", c.length);
        }
        for c in &chunks {
            assert!(c.length <= MAX_SIZE, "chunk above max: {}", c.length);
        }
        // Offsets are contiguous.
        let mut expect = 0u64;
        for c in &chunks {
            assert_eq!(c.offset, expect);
            expect += c.length as u64;
        }
    }

    #[test]
    fn empty_file_yields_no_chunks() {
        let (chunks, oid, size) = chunk_all(&[]);
        assert!(chunks.is_empty());
        assert_eq!(size, 0);
        assert_eq!(oid, blake3::hash(&[]));
    }

    #[test]
    fn reusable_workspace_matches_streamcdc_across_sizes_and_short_reads() {
        use fastcdc::v2020::StreamCDC;

        struct ShortReads<'a>(&'a [u8]);
        impl Read for ShortReads<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let limit = out.len().min(997);
                self.0.read(&mut out[..limit])
            }
        }

        let data = test_data(20 * 1024 * 1024, 82);
        let mut workspace = Chunker::default();
        for params in [
            ChunkParams::default(),
            ChunkParams {
                min: 64,
                avg: 256,
                max: 1024,
            },
            ChunkParams {
                min: 1024,
                avg: 4096,
                max: 16385,
            },
        ] {
            // Large -> tiny -> empty also exercises reuse without stale bytes.
            for len in [data.len(), params.max as usize, 8192, 512, 0] {
                let source = &data[..len];
                let expected: Vec<_> = StreamCDC::new(
                    source,
                    params.min as usize,
                    params.avg as usize,
                    params.max as usize,
                )
                .map(|entry| {
                    let entry = entry.unwrap();
                    Chunk {
                        hash: blake3::hash(&entry.data),
                        offset: entry.offset,
                        length: entry.length as u32,
                    }
                })
                .collect();
                let (actual, oid, size) = workspace
                    .chunk(ShortReads(source), params, |c, bytes| {
                        assert_eq!(bytes, &source[c.offset as usize..][..c.length as usize]);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(actual, expected, "params={params:?}, len={len}");
                assert_eq!(oid, blake3::hash(source));
                assert_eq!(size, len as u64);
            }
        }
    }

    #[test]
    fn workspace_growth_is_bounded_and_tiny_files_stay_small() {
        let mut workspace = Chunker::default();
        for _ in 0..100 {
            workspace
                .chunk(&[7u8; 512][..], ChunkParams::default(), |_, _| Ok(()))
                .unwrap();
            assert!(workspace.buffer.capacity() <= 8192);
        }
        let params = ChunkParams {
            min: 1024,
            avg: 4096,
            max: 16385,
        };
        let mut workspace = Chunker::default();
        workspace
            .chunk(std::io::repeat(7).take(1024 * 1024), params, |_, _| Ok(()))
            .unwrap();
        assert!(workspace.buffer.capacity() <= 2 * params.max as usize);
    }

    #[test]
    fn file_exactly_max_chunk_size_round_trips() {
        let data = test_data(MAX_SIZE as usize, 21);
        let (chunks, oid, size) = chunk_all(&data);
        assert_eq!(size, MAX_SIZE as u64);
        assert_eq!(oid, blake3::hash(&data));
        assert_eq!(
            chunks.iter().map(|c| c.length as u64).sum::<u64>(),
            MAX_SIZE as u64
        );
    }

    #[test]
    fn file_smaller_than_min_is_one_chunk() {
        let data = test_data(1000, 7);
        let (chunks, _, size) = chunk_all(&data);
        assert_eq!(chunks.len(), 1);
        assert_eq!(size, 1000);
        assert_eq!(chunks[0].hash, blake3::hash(&data));
    }

    #[test]
    fn sink_sees_exact_chunk_bytes() {
        let data = test_data(3 * 1024 * 1024, 9);
        let mut rebuilt = Vec::new();
        let (chunks, _, _) = chunk_stream(&data[..], ChunkParams::default(), |c, bytes| {
            assert_eq!(blake3::hash(bytes), c.hash);
            assert_eq!(bytes.len() as u32, c.length);
            rebuilt.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();
        assert!(!chunks.is_empty());
        assert_eq!(rebuilt, data);
    }

    #[test]
    fn params_validation() {
        assert!(ChunkParams::default().validate().is_ok());
        // fastcdc hard-bound edges are accepted…
        assert!(
            ChunkParams {
                min: 64,
                avg: 256,
                max: 1024
            }
            .validate()
            .is_ok()
        );
        assert!(
            ChunkParams {
                min: 1 << 20,
                avg: 4 << 20,
                max: 16 << 20
            }
            .validate()
            .is_ok()
        );
        // …out-of-range values are rejected, naming the config key:
        let err = ChunkParams {
            min: 63,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("cdc.chunk.min"), "{err}");
        let err = ChunkParams {
            max: (16 << 20) + 1,
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("cdc.chunk.max"), "{err}");
        // …and misordered sizes too:
        let err = ChunkParams {
            min: 1 << 20,
            avg: 512 * 1024,
            max: 8 << 20,
        }
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("min <= avg <= max"), "{err}");
    }

    #[test]
    fn custom_params_change_chunking() {
        let data = test_data(4 * 1024 * 1024, 5);
        let small = ChunkParams {
            min: 64 * 1024,
            avg: 256 * 1024,
            max: 1024 * 1024,
        };
        let (chunks, oid, _) = chunk_stream(&data[..], small, |_, _| Ok(())).unwrap();
        let (default_chunks, default_oid, _) = chunk_all(&data);
        assert!(
            chunks.len() > default_chunks.len(),
            "smaller bounds → more chunks"
        );
        for c in &chunks {
            assert!(
                c.length <= small.max,
                "chunk above configured max: {}",
                c.length
            );
        }
        assert_eq!(oid, default_oid, "oid is chunking-independent");
    }

    #[test]
    fn small_edit_changes_few_chunks() {
        let mut data = test_data(20 * 1024 * 1024, 1);
        let (before, _, _) = chunk_all(&data);
        data[10 * 1024 * 1024] ^= 0xFF; // 1-byte edit in the middle
        let (after, _, _) = chunk_all(&data);

        let before_set: std::collections::HashSet<_> = before.iter().map(|c| c.hash).collect();
        let changed = after
            .iter()
            .filter(|c| !before_set.contains(&c.hash))
            .count();
        assert!(changed <= 2, "1-byte edit changed {changed} chunks");
    }
}
