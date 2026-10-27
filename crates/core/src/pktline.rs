//! git pkt-line framing (gitprotocol-common(5)), as used by the
//! filter-process protocol: 4 ASCII hex digits of total length (including
//! the 4 prefix bytes), then payload; `0000` is the flush packet.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};

/// Largest payload one packet may carry (65520 total - 4 prefix).
pub const MAX_PAYLOAD: usize = 65516;

/// One packet: `Some(payload)` for data, `None` for flush.
pub fn read_packet(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let Some(len) = read_length(reader)? else {
        return Ok(None);
    };
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .context("reading pkt-line payload")?;
    Ok(Some(payload))
}

fn read_length(reader: &mut impl Read) -> Result<Option<usize>> {
    let mut prefix = [0u8; 4];
    reader
        .read_exact(&mut prefix)
        .context("reading pkt-line length")?;
    let len = usize::from_str_radix(
        std::str::from_utf8(&prefix).context("non-ascii pkt-line length")?,
        16,
    )
    .context("bad pkt-line length")?;
    match len {
        0 => Ok(None), // flush
        1..=3 => bail!("invalid pkt-line length {len}"),
        _ if len - 4 > MAX_PAYLOAD => bail!("pkt-line payload {len} too large"),
        _ => Ok(Some(len - 4)),
    }
}

pub fn write_packet(writer: &mut impl Write, payload: &[u8]) -> Result<()> {
    // Callers hand arbitrary sizes; split to fit the wire limit.
    for part in payload.chunks(MAX_PAYLOAD.max(1)) {
        write!(writer, "{:04x}", part.len() + 4)?;
        writer.write_all(part)?;
    }
    Ok(())
}

pub fn write_flush(writer: &mut impl Write) -> Result<()> {
    writer.write_all(b"0000")?;
    writer.flush()?;
    Ok(())
}

/// Text convenience: `key=value` lines carry a trailing `\n` on the wire.
pub fn write_text(writer: &mut impl Write, text: &str) -> Result<()> {
    write_packet(writer, format!("{text}\n").as_bytes())
}

pub fn read_text(reader: &mut impl Read) -> Result<Option<String>> {
    Ok(read_packet(reader)?.map(|p| {
        let mut s = String::from_utf8(p)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        if s.ends_with('\n') {
            s.pop();
        }
        s
    }))
}

/// `Read` over the content packets of one file, ending at the flush packet.
/// Reads payloads directly into the caller's buffer without packet allocations.
pub struct PktReader<'a, R: Read> {
    inner: &'a mut R,
    remaining: usize,
    done: bool,
}

impl<'a, R: Read> PktReader<'a, R> {
    pub fn new(inner: &'a mut R) -> Self {
        PktReader {
            inner,
            remaining: 0,
            done: false,
        }
    }

    /// Consume any unread remainder through the terminating flush, so the
    /// stream is positioned for the next protocol message.
    pub fn drain(mut self) -> Result<()> {
        std::io::copy(&mut self, &mut std::io::sink())?;
        Ok(())
    }
}

impl<R: Read> Read for PktReader<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.remaining == 0 && !self.done {
            match read_length(self.inner).map_err(std::io::Error::other)? {
                Some(len) => self.remaining = len,
                None => self.done = true,
            }
        }
        if self.done {
            return Ok(0);
        }
        let n = out.len().min(self.remaining);
        self.inner.read_exact(&mut out[..n])?;
        self.remaining -= n;
        Ok(n)
    }
}

/// `Write` emitting content packets (no flush — the protocol layer decides
/// when the file ends).
pub struct PktWriter<'a, W: Write> {
    inner: &'a mut W,
}

impl<'a, W: Write> PktWriter<'a, W> {
    pub fn new(inner: &'a mut W) -> Self {
        PktWriter { inner }
    }
}

impl<W: Write> Write for PktWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !buf.is_empty() {
            write_packet(self.inner, buf).map_err(std::io::Error::other)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_round_trip() {
        let mut wire = Vec::new();
        write_packet(&mut wire, b"hello").unwrap();
        write_flush(&mut wire).unwrap();
        let mut r = &wire[..];
        assert_eq!(read_packet(&mut r).unwrap().unwrap(), b"hello");
        assert_eq!(read_packet(&mut r).unwrap(), None);
    }

    #[test]
    fn text_lines_carry_newline_on_wire() {
        let mut wire = Vec::new();
        write_text(&mut wire, "status=success").unwrap();
        assert_eq!(&wire[..4], b"0013"); // 4 + 15
        let mut r = &wire[..];
        assert_eq!(read_text(&mut r).unwrap().unwrap(), "status=success");
    }

    #[test]
    fn oversized_write_splits_into_max_packets() {
        let data = vec![7u8; MAX_PAYLOAD + 10];
        let mut wire = Vec::new();
        write_packet(&mut wire, &data).unwrap();
        let mut r = &wire[..];
        assert_eq!(read_packet(&mut r).unwrap().unwrap().len(), MAX_PAYLOAD);
        assert_eq!(read_packet(&mut r).unwrap().unwrap().len(), 10);
    }

    #[test]
    fn pkt_reader_streams_until_flush() {
        let mut wire = Vec::new();
        write_packet(&mut wire, &vec![1u8; MAX_PAYLOAD]).unwrap();
        write_packet(&mut wire, b"tail").unwrap();
        write_flush(&mut wire).unwrap();
        write_text(&mut wire, "next=message").unwrap();

        let mut r = &wire[..];
        let mut content = Vec::new();
        PktReader::new(&mut r).read_to_end(&mut content).unwrap();
        assert_eq!(content.len(), MAX_PAYLOAD + 4);
        // Stream is positioned exactly after the flush.
        assert_eq!(read_text(&mut r).unwrap().unwrap(), "next=message");
    }

    #[test]
    fn rejects_malformed_lengths() {
        assert!(read_packet(&mut &b"0001"[..]).is_err());
        assert!(read_packet(&mut &b"zzzz"[..]).is_err());
        assert!(read_packet(&mut &b"fff5"[..]).is_err()); // > MAX_PAYLOAD
    }

    #[test]
    fn partial_packet_drain_stops_at_flush_and_skips_empty_packets() {
        let mut wire = b"0004".to_vec(); // legal empty data packet
        write_packet(&mut wire, b"content").unwrap();
        write_flush(&mut wire).unwrap();
        write_text(&mut wire, "next=message").unwrap();
        let mut source = wire.as_slice();
        let mut reader = PktReader::new(&mut source);
        assert_eq!(reader.read(&mut []).unwrap(), 0);
        let mut head = [0; 2];
        reader.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"co");
        reader.drain().unwrap();
        assert_eq!(
            read_text(&mut source).unwrap().as_deref(),
            Some("next=message")
        );
    }

    #[test]
    fn truncated_packet_is_an_error() {
        let mut source = &b"0008abc"[..];
        assert!(
            PktReader::new(&mut source)
                .read_to_end(&mut Vec::new())
                .is_err()
        );
    }
}
