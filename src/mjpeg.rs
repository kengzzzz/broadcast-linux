//! Decodes MJPEG frames that carry restart markers on several threads.
//!
//! Each restart interval decodes independently, but intervals rarely line up with
//! rows of MCUs. So each band of intervals becomes its own JPEG whose rows are one
//! interval wide; the decoded MCUs are then copied back to their place in the frame.

use std::thread;

use turbojpeg::{Decompressor, Subsamp, YuvImage};

/// libjpeg's largest image side.
const MAX_SIDE: usize = 65_500;

#[derive(Clone, Copy)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    pub subsamp: Subsamp,
}

impl Geometry {
    fn factors(self) -> (usize, usize) {
        match self.subsamp {
            Subsamp::Sub2x2 => (2, 2),
            Subsamp::Sub2x1 => (2, 1),
            _ => (1, 1),
        }
    }
}

struct Split<'a> {
    header: &'a [u8],
    /// Offset of the frame height in the SOF segment; the width follows it.
    dims: usize,
    interval: usize,
    chunks: Vec<&'a [u8]>,
}

fn split(frame: &[u8]) -> Option<Split<'_>> {
    let mut i = 2;
    let (mut dims, mut interval) = (None, 0);
    let scan = loop {
        let segment = frame.get(i..i + 4)?;
        if segment[0] != 0xFF {
            return None;
        }
        let marker = segment[1];
        let len = usize::from(u16::from_be_bytes([segment[2], segment[3]]));
        match marker {
            0xC0 | 0xC1 => dims = Some(i + 5),
            0xC2..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => return None,
            0xDD => {
                interval = usize::from(u16::from_be_bytes(
                    frame.get(i + 4..i + 6)?.try_into().ok()?,
                ));
            }
            _ => {}
        }
        i += 2 + len;
        if marker == 0xDA {
            break i;
        }
    };
    if interval == 0 {
        return None;
    }
    let mut chunks = Vec::new();
    let (mut start, mut j) = (scan, scan);
    while j + 1 < frame.len() {
        if frame[j] == 0xFF {
            match frame[j + 1] {
                0xD0..=0xD7 => {
                    chunks.push(&frame[start..j]);
                    start = j + 2;
                    j += 2;
                    continue;
                }
                0x00 | 0xFF => {}
                0xD9 => break,
                // A second scan or other segment: leave it to the plain decoder.
                _ => return None,
            }
        }
        j += 1;
    }
    chunks.push(&frame[start..j]);
    Some(Split {
        header: &frame[..scan],
        dims: dims?,
        interval,
        chunks,
    })
}

/// Each band is its own JPEG, so its restart markers are renumbered from 0.
fn band(split: &Split, chunks: &[&[u8]], width: usize, height: usize, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(split.header);
    let side = |v: usize| u16::try_from(v).unwrap_or(u16::MAX).to_be_bytes();
    out[split.dims..split.dims + 2].copy_from_slice(&side(height));
    out[split.dims + 2..split.dims + 4].copy_from_slice(&side(width));
    for (n, chunk) in chunks.iter().enumerate() {
        if n > 0 {
            out.extend_from_slice(&[0xFF, 0xD0 + u8::try_from((n - 1) % 8).unwrap_or(0)]);
        }
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
}

pub struct Worker {
    jpeg: Decompressor,
    band: Vec<u8>,
    pixels: Vec<u8>,
}

impl Worker {
    pub fn new() -> turbojpeg::Result<Self> {
        Ok(Self {
            jpeg: Decompressor::new()?,
            band: Vec::new(),
            pixels: Vec::new(),
        })
    }
}

/// The output planes, shared by the workers. Each MCU belongs to exactly one
/// restart interval and so to one worker, which makes their writes disjoint.
#[derive(Clone, Copy)]
struct Planes {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: see Planes; workers only write MCUs of their own intervals.
unsafe impl Send for Planes {}
// SAFETY: as above.
unsafe impl Sync for Planes {}

/// Decodes `frame` into planar YUV `out`. `None` if the frame cannot be split this
/// way, `Some(false)` if a band does not decode.
pub fn decode(
    frame: &[u8],
    geometry: Geometry,
    out: &mut [u8],
    workers: &mut [Worker],
) -> Option<bool> {
    let split = split(frame)?;
    let (hs, vs) = geometry.factors();
    let (mcu_w, mcu_h) = (8 * hs, 8 * vs);
    let per_row = geometry.width.div_ceil(mcu_w);
    let total = per_row * geometry.height.div_ceil(mcu_h);
    let interval = split.interval;
    if split.chunks.len() != total.div_ceil(interval) || interval * mcu_w > MAX_SIDE {
        return None;
    }
    let (cw, ch) = (geometry.width / hs, geometry.height / vs);
    if out.len() != geometry.width * geometry.height + 2 * cw * ch {
        return None;
    }
    let planes = Planes {
        ptr: out.as_mut_ptr(),
        len: out.len(),
    };
    let per_worker = split.chunks.len().div_ceil(workers.len());
    let decoded = thread::scope(|s| {
        let split = &split;
        let jobs: Vec<_> = workers
            .iter_mut()
            .zip((0..split.chunks.len()).step_by(per_worker))
            .map(|(worker, first)| {
                let last = (first + per_worker).min(split.chunks.len());
                s.spawn(move || {
                    // The final interval may be short, so it gets a band of its own.
                    let full_end = last.min(total / interval).max(first);
                    let max_rows = MAX_SIDE / mcu_h;
                    let mut bands: Vec<(usize, usize)> = (first..full_end)
                        .step_by(max_rows)
                        .map(|a| (a, (a + max_rows).min(full_end)))
                        .collect();
                    if full_end < last {
                        bands.push((full_end, last));
                    }
                    bands
                        .into_iter()
                        .all(|(a, b)| decode_band(split, a, b, geometry, total, planes, worker))
                })
            })
            .collect();
        jobs.into_iter().all(|job| job.join().unwrap_or(false))
    });
    Some(decoded)
}

fn decode_band(
    split: &Split,
    first: usize,
    end: usize,
    geometry: Geometry,
    total: usize,
    planes: Planes,
    worker: &mut Worker,
) -> bool {
    let (hs, vs) = geometry.factors();
    let (mcu_w, mcu_h) = (8 * hs, 8 * vs);
    let interval = split.interval;
    let mcus = interval.min(total - first * interval);
    let (bw, bh) = (mcus * mcu_w, (end - first) * mcu_h);
    band(split, &split.chunks[first..end], bw, bh, &mut worker.band);
    let (bcw, bch) = (bw / hs, bh / vs);
    worker.pixels.resize(bw * bh + 2 * bcw * bch, 0);
    let image = YuvImage {
        pixels: worker.pixels.as_mut_slice(),
        width: bw,
        align: 1,
        height: bh,
        subsamp: geometry.subsamp,
    };
    if worker.jpeg.decompress_to_yuv(&worker.band, image).is_err() {
        return false;
    }
    let (width, height) = (geometry.width, geometry.height);
    let (cw, ch) = (width / hs, height / vs);
    let per_row = width.div_ceil(mcu_w);
    // Plane offsets and widths in the band and in the frame; block size per plane.
    let layout = [
        (0, bw, 0, width, height, mcu_w, mcu_h),
        (bw * bh, bcw, width * height, cw, ch, mcu_w / hs, mcu_h / vs),
        (
            bw * bh + bcw * bch,
            bcw,
            width * height + cw * ch,
            cw,
            ch,
            mcu_w / hs,
            mcu_h / vs,
        ),
    ];
    for row in 0..end - first {
        let row_mcus = mcus.min(total - (first + row) * interval);
        let mut done = 0;
        while done < row_mcus {
            let index = (first + row) * interval + done;
            let (gx, gy) = (index % per_row, index / per_row);
            let run = (per_row - gx).min(row_mcus - done);
            for &(src_base, src_w, dst_base, dst_w, dst_h, block_w, block_h) in &layout {
                let left = gx * block_w;
                let len = (run * block_w).min(dst_w - left);
                for line in 0..block_h {
                    let top = gy * block_h + line;
                    if top >= dst_h {
                        break;
                    }
                    let src = src_base + (row * block_h + line) * src_w + done * block_w;
                    let dst = dst_base + top * dst_w + left;
                    assert!(dst + len <= planes.len && src + len <= worker.pixels.len());
                    // SAFETY: in bounds (checked above); this MCU run belongs to this
                    // band only, so no other worker writes these bytes.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            worker.pixels.as_ptr().add(src),
                            planes.ptr.add(dst),
                            len,
                        );
                    }
                }
            }
            done += run;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use turbojpeg::raw;

    fn encode(width: usize, height: usize, subsamp: Subsamp, restart: i32) -> (Vec<u8>, Vec<u8>) {
        let pixels: Vec<u8> = (0..width * height * 3)
            .map(|i| u8::try_from((i * 7 + i / (width * 3) * 13) % 251).unwrap())
            .collect();
        // SAFETY: plain TurboJPEG calls on a handle and buffers owned by this test.
        unsafe {
            let handle = raw::tj3Init(raw::TJINIT_TJINIT_COMPRESS.cast_signed());
            raw::tj3Set(handle, raw::TJPARAM_TJPARAM_QUALITY.cast_signed(), 85);
            raw::tj3Set(
                handle,
                raw::TJPARAM_TJPARAM_SUBSAMP.cast_signed(),
                subsamp as i32,
            );
            raw::tj3Set(
                handle,
                raw::TJPARAM_TJPARAM_RESTARTBLOCKS.cast_signed(),
                restart,
            );
            let (mut buf, mut size) = (std::ptr::null_mut(), 0);
            let status = raw::tj3Compress8(
                handle,
                pixels.as_ptr(),
                i32::try_from(width).unwrap(),
                0,
                i32::try_from(height).unwrap(),
                raw::TJPF_TJPF_RGB,
                &raw mut buf,
                &raw mut size,
            );
            assert_eq!(status, 0);
            let jpeg = std::slice::from_raw_parts(buf, usize::try_from(size).unwrap()).to_vec();
            raw::tj3Free(buf.cast());
            raw::tj3Destroy(handle);
            let mut plain = vec![0; turbojpeg::yuv_pixels_len(width, 1, height, subsamp).unwrap()];
            Decompressor::new()
                .unwrap()
                .decompress_to_yuv(
                    &jpeg,
                    YuvImage {
                        pixels: plain.as_mut_slice(),
                        width,
                        align: 1,
                        height,
                        subsamp,
                    },
                )
                .unwrap();
            (jpeg, plain)
        }
    }

    #[test]
    fn matches_the_plain_decoder() {
        let mut workers: Vec<Worker> = (0..3).map(|_| Worker::new().unwrap()).collect();
        for subsamp in [Subsamp::Sub2x1, Subsamp::Sub2x2, Subsamp::None] {
            for (width, height) in [(1920, 1080), (1366, 768), (640, 360), (98, 34)] {
                for restart in [1, 7, 1005] {
                    let (jpeg, plain) = encode(width, height, subsamp, restart);
                    let mut out = vec![0; plain.len()];
                    let geometry = Geometry {
                        width,
                        height,
                        subsamp,
                    };
                    let decoded = decode(&jpeg, geometry, &mut out, &mut workers);
                    assert_eq!(
                        decoded,
                        Some(true),
                        "{subsamp:?} {width}x{height} every {restart}"
                    );
                    assert!(out == plain, "{subsamp:?} {width}x{height} every {restart}");
                }
            }
        }
    }

    #[test]
    fn leaves_frames_without_restarts_alone() {
        let (jpeg, plain) = encode(64, 32, Subsamp::Sub2x1, 0);
        let mut out = vec![0; plain.len()];
        let geometry = Geometry {
            width: 64,
            height: 32,
            subsamp: Subsamp::Sub2x1,
        };
        assert!(decode(&jpeg, geometry, &mut out, &mut [Worker::new().unwrap()]).is_none());
    }
}
