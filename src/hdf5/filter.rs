//! The HDF5 filter pipeline, inverted on read.

use super::{err, FilterDef, Result};
use flate2::read::ZlibDecoder;
use std::io::Read;

pub const DEFLATE: u16 = 1;
pub const SHUFFLE: u16 = 2;
pub const FLETCHER32: u16 = 3;

/// Undo a filter pipeline. Filters are recorded in write order, so they are
/// reversed here.
pub fn decode(filters: &[FilterDef], mut data: Vec<u8>, elem_size: usize) -> Result<Vec<u8>> {
    for f in filters.iter().rev() {
        data = match f.id {
            DEFLATE => inflate(&data)?,
            SHUFFLE => unshuffle(data, elem_size),
            FLETCHER32 => {
                // A 4-byte checksum is appended to the data; trust and drop it.
                if data.len() >= 4 {
                    data.truncate(data.len() - 4);
                }
                data
            }
            other => {
                if f.optional {
                    data
                } else {
                    return err(format!("unsupported HDF5 filter id {other}"));
                }
            }
        };
    }
    Ok(data)
}

/// Ceiling on what one chunk may inflate to.
///
/// A deflate stream can expand by a thousandfold or more, and the compressed
/// size is the only thing the file bounds - so without a limit a small damaged
/// or hostile chunk inflates until the allocator gives up, which aborts the
/// process rather than failing the request. A chunk here is a tile of one
/// variable: a whole SEVIRI disc at two bytes a pixel is about 27 MB, so 64 MB
/// is far above anything genuine.
const MAX_INFLATED_BYTES: u64 = 64 * 1024 * 1024;

fn inflate(src: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    // `take` stops the reader at the limit rather than after the fact, so the
    // memory is never allocated in the first place.
    let mut reader = ZlibDecoder::new(src).take(MAX_INFLATED_BYTES + 1);
    reader
        .read_to_end(&mut out)
        .map_err(|e| super::Error(format!("deflate: {e}")))?;
    if out.len() as u64 > MAX_INFLATED_BYTES {
        return err(format!(
            "a chunk inflated past the {} MB limit; the file is damaged",
            MAX_INFLATED_BYTES / (1024 * 1024)
        ));
    }
    Ok(out)
}

/// The shuffle filter groups the Nth byte of every element together; this puts
/// the bytes back in element order.
fn unshuffle(src: Vec<u8>, elem: usize) -> Vec<u8> {
    if elem <= 1 || src.len() <= elem {
        return src;
    }
    let n = src.len() / elem; // whole elements
    let tail = src.len() % elem;
    let mut out = vec![0u8; src.len()];
    let mut p = 0usize;
    for b in 0..elem {
        for i in 0..n {
            out[i * elem + b] = src[p];
            p += 1;
        }
    }
    if tail > 0 {
        let start = n * elem;
        out[start..].copy_from_slice(&src[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn zlib(bytes: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::best());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn round_trips_a_normal_chunk() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(inflate(&zlib(&data)).unwrap(), data);
    }

    /// Highly compressible input is exactly what a damaged or hostile chunk
    /// looks like: a few kilobytes that inflate to hundreds of megabytes.
    #[test]
    fn a_decompression_bomb_is_refused() {
        let bomb = zlib(&vec![0u8; (MAX_INFLATED_BYTES + 1_000_000) as usize]);
        assert!(
            bomb.len() < 200_000,
            "test bomb should be small: {} bytes",
            bomb.len()
        );
        let e = inflate(&bomb).unwrap_err();
        assert!(e.0.contains("limit"), "unexpected error: {}", e.0);
    }

    #[test]
    fn unshuffle_inverts_the_shuffle() {
        // Two 4-byte elements, shuffled: all first bytes, then all seconds...
        let shuffled = vec![1, 5, 2, 6, 3, 7, 4, 8];
        assert_eq!(unshuffle(shuffled, 4), vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn unshuffle_keeps_a_trailing_partial_element() {
        let src = vec![1, 4, 2, 5, 3, 6, 9];
        let out = unshuffle(src.clone(), 3);
        assert_eq!(out.len(), src.len());
        assert_eq!(out[6], 9, "the tail must survive");
    }
}
