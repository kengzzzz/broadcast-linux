use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use std::thread;

use anyhow::{Context, Result, bail};
use turbojpeg::{Decompressor, Subsamp, YuvImage};

use crate::config::ParallelDecode;
use crate::mjpeg;
use crate::webcam::Format;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Yuyv,
    Nv12,
    J420,
    J422,
    J444,
}

impl Layout {
    pub fn worker_name(self) -> &'static str {
        match self {
            Self::Yuyv => "yuyv",
            Self::Nv12 => "nv12",
            Self::J420 => "j420",
            Self::J422 => "j422",
            Self::J444 => "j444",
        }
    }

    fn subsamp(self) -> Option<Subsamp> {
        match self {
            Self::J420 => Some(Subsamp::Sub2x2),
            Self::J422 => Some(Subsamp::Sub2x1),
            Self::J444 => Some(Subsamp::None),
            Self::Yuyv | Self::Nv12 => None,
        }
    }

    fn frame_bytes(self, width: usize, height: usize) -> usize {
        let pixels = width * height;
        match self {
            Self::Nv12 | Self::J420 => pixels * 3 / 2,
            Self::Yuyv | Self::J422 => pixels * 2,
            Self::J444 => pixels * 3,
        }
    }
}

pub const SLOTS: usize = 2;

/// Shared memory holding `SLOTS` input frames, then `SLOTS` output frames. Pipes pass
/// one-byte slot numbers, and whoever holds a slot's number is the only one using it.
pub struct SharedFrames {
    fd: OwnedFd,
    ptr: NonNull<u8>,
    in_bytes: usize,
    out_bytes: usize,
}

// SAFETY: the mapping is only reached through slots, which the token protocol gives to one
// thread or process at a time.
unsafe impl Send for SharedFrames {}
// SAFETY: as above.
unsafe impl Sync for SharedFrames {}

impl SharedFrames {
    pub fn new(in_bytes: usize, out_bytes: usize) -> Result<Self> {
        // SAFETY: plain syscall with a NUL-terminated name.
        let raw =
            unsafe { libc::memfd_create(c"broadcast-linux-frames".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            bail!("creating shared frames: {}", io::Error::last_os_error());
        }
        // SAFETY: memfd_create returned a new descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let len = SLOTS * (in_bytes + out_bytes);
        // SAFETY: plain syscall on the descriptor owned above.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), libc::off_t::try_from(len)?) } < 0 {
            bail!("sizing shared frames: {}", io::Error::last_os_error());
        }
        // SAFETY: maps the whole file just sized; unmapped in Drop.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            bail!("mapping shared frames: {}", io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            ptr: NonNull::new(ptr.cast()).context("shared frames mapped at null")?,
            in_bytes,
            out_bytes,
        })
    }

    pub fn path(&self) -> String {
        format!("/proc/{}/fd/{}", std::process::id(), self.fd.as_raw_fd())
    }

    /// # Safety
    /// The caller must hold slot `slot`'s token.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn input(&self, slot: usize) -> &mut [u8] {
        assert!(slot < SLOTS);
        // SAFETY: in bounds of the mapping; the token makes this the only reference.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.ptr.as_ptr().add(slot * self.in_bytes),
                self.in_bytes,
            )
        }
    }

    /// # Safety
    /// The caller must hold slot `slot`'s token.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn output(&self, slot: usize) -> &mut [u8] {
        assert!(slot < SLOTS);
        let offset = SLOTS * self.in_bytes + slot * self.out_bytes;
        // SAFETY: in bounds of the mapping; the token makes this the only reference.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(offset), self.out_bytes) }
    }
}

impl Drop for SharedFrames {
    fn drop(&mut self) {
        let len = SLOTS * (self.in_bytes + self.out_bytes);
        // SAFETY: unmaps the region mapped in new(); no slot references outlive self.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), len) };
    }
}

pub struct Decoder {
    jpeg: Decompressor,
    width: usize,
    height: usize,
    layout: Layout,
    planes: Vec<u8>,
    luma: [u8; 256],
    chroma: [u8; 256],
    warned: bool,
    workers: Vec<mjpeg::Worker>,
}

impl Decoder {
    pub fn new(
        format: Format,
        first: &[u8],
        width: u32,
        height: u32,
        fps: u32,
        parallel: ParallelDecode,
    ) -> Result<Self> {
        // Up to 1080p30 one thread decodes in a few milliseconds; splitting the frame
        // costs more CPU in total, so by default only heavier streams get the threads.
        let wanted = match parallel {
            ParallelDecode::On => true,
            ParallelDecode::Off => false,
            ParallelDecode::Auto => {
                u64::from(width) * u64::from(height) * u64::from(fps) > 1920 * 1080 * 30
            }
        };
        let threads = if wanted && format == Format::Mjpeg {
            thread::available_parallelism()
                .map_or(1, usize::from)
                .min(8)
        } else {
            1
        };
        let workers = if threads > 1 {
            (0..threads)
                .map(|_| mjpeg::Worker::new())
                .collect::<turbojpeg::Result<_>>()?
        } else {
            Vec::new()
        };
        let (width, height) = (width as usize, height as usize);
        let mut jpeg = Decompressor::new()?;
        let layout = match format {
            Format::Yuyv => Layout::Yuyv,
            Format::Nv12 => Layout::Nv12,
            Format::Mjpeg => jpeg_layout(&mut jpeg, first, width, height)?,
        };
        let (luma, chroma) = range_tables();
        Ok(Self {
            jpeg,
            width,
            height,
            layout,
            planes: Vec::new(),
            luma,
            chroma,
            warned: false,
            workers,
        })
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn worker_bytes(&self) -> usize {
        self.layout.frame_bytes(self.width, self.height)
    }

    /// Writes the frame in this stream's layout to `out`; false for a frame to skip.
    pub fn fill_for_worker(&mut self, frame: &[u8], out: &mut [u8]) -> Result<bool> {
        if self.layout.subsamp().is_none() {
            let Some(frame) = frame.get(..out.len()) else {
                return Ok(false);
            };
            out.copy_from_slice(frame);
            return Ok(true);
        }
        self.decode(frame, out)
    }

    /// Writes the frame as limited-range YUYV to `out`; false for a frame to skip.
    pub fn fill_for_loopback(&mut self, frame: &[u8], out: &mut [u8]) -> Result<bool> {
        let (width, height) = (self.width, self.height);
        match self.layout {
            Layout::Yuyv => {
                let Some(frame) = frame.get(..out.len()) else {
                    return Ok(false);
                };
                out.copy_from_slice(frame);
            }
            Layout::Nv12 => {
                let Some(frame) = frame.get(..width * height * 3 / 2) else {
                    return Ok(false);
                };
                let (luma, uv) = frame.split_at(width * height);
                for (row, out) in out.chunks_exact_mut(width * 2).enumerate() {
                    let y = &luma[row * width..][..width];
                    let uv = &uv[row / 2 * width..][..width];
                    for ((out, y), uv) in out
                        .as_chunks_mut::<4>()
                        .0
                        .iter_mut()
                        .zip(y.as_chunks::<2>().0)
                        .zip(uv.as_chunks::<2>().0)
                    {
                        *out = [y[0], uv[0], y[1], uv[1]];
                    }
                }
            }
            Layout::J420 | Layout::J422 | Layout::J444 => {
                let mut planes = std::mem::take(&mut self.planes);
                planes.resize(self.worker_bytes(), 0);
                let decoded = self.decode(frame, &mut planes)?;
                if decoded {
                    self.pack_planar(&planes, out);
                }
                self.planes = planes;
                return Ok(decoded);
            }
        }
        Ok(true)
    }

    /// Decodes MJPEG to planar YUV in `out`; false for a frame that does not decode.
    fn decode(&mut self, frame: &[u8], out: &mut [u8]) -> Result<bool> {
        match jpeg_layout(&mut self.jpeg, frame, self.width, self.height) {
            Ok(layout) if layout == self.layout => {}
            Ok(layout) => bail!(
                "the webcam switched from {} to {} mid-stream",
                self.layout.worker_name(),
                layout.worker_name()
            ),
            Err(e) => {
                self.warn_once(&e);
                return Ok(false);
            }
        }
        let subsamp = self.layout.subsamp().unwrap_or(Subsamp::None);
        if !self.workers.is_empty() {
            let geometry = mjpeg::Geometry {
                width: self.width,
                height: self.height,
                subsamp,
            };
            if let Some(decoded) = mjpeg::decode(frame, geometry, out, &mut self.workers) {
                if !decoded {
                    self.warn_once(&anyhow::anyhow!("part of the frame is damaged"));
                }
                return Ok(decoded);
            }
        }
        let image = YuvImage {
            pixels: out,
            width: self.width,
            align: 1,
            height: self.height,
            subsamp,
        };
        if let Err(e) = self.jpeg.decompress_to_yuv(frame, image) {
            self.warn_once(&e.into());
            return Ok(false);
        }
        Ok(true)
    }

    fn warn_once(&mut self, e: &anyhow::Error) {
        if !self.warned {
            self.warned = true;
            eprintln!("camera: skipping MJPEG frames that do not decode: {e}");
        }
    }

    fn pack_planar(&self, planes: &[u8], out: &mut [u8]) {
        let (width, height) = (self.width, self.height);
        let (chroma_width, chroma_rows) = match self.layout {
            Layout::J420 => (width / 2, 2),
            Layout::J422 => (width / 2, 1),
            _ => (width, 1),
        };
        let chroma_height = height / chroma_rows;
        let (luma, rest) = planes.split_at(width * height);
        let (u_plane, v_plane) = rest.split_at(chroma_width * chroma_height);
        let (ly, lc) = (&self.luma, &self.chroma);
        for (row, out) in out.chunks_exact_mut(width * 2).enumerate() {
            let y = &luma[row * width..][..width];
            // 4:2:0 chroma sits between two luma rows: weigh the nearer chroma row 3:1.
            let near = row / chroma_rows;
            let far = match (chroma_rows, row % 2) {
                (1, _) => near,
                (_, 0) => near.saturating_sub(1),
                _ => (near + 1).min(chroma_height - 1),
            };
            let at = |plane: &[u8], x: usize| {
                let (a, b) = (
                    plane[near * chroma_width + x],
                    plane[far * chroma_width + x],
                );
                u8::try_from((3 * u16::from(a) + u16::from(b) + 2) / 4).unwrap_or(u8::MAX)
            };
            for (x, out) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (cu, cv) = if chroma_width == width {
                    (
                        at(u_plane, 2 * x).midpoint(at(u_plane, 2 * x + 1)),
                        at(v_plane, 2 * x).midpoint(at(v_plane, 2 * x + 1)),
                    )
                } else {
                    (at(u_plane, x), at(v_plane, x))
                };
                *out = [
                    ly[usize::from(y[2 * x])],
                    lc[usize::from(cu)],
                    ly[usize::from(y[2 * x + 1])],
                    lc[usize::from(cv)],
                ];
            }
        }
    }
}

fn range_tables() -> ([u8; 256], [u8; 256]) {
    let table = |scale: u32, offset: u32| {
        std::array::from_fn(|v| {
            let v = u32::try_from(v).unwrap_or(0);
            u8::try_from((v * scale + offset) / 255).unwrap_or(u8::MAX)
        })
    };
    // 16 + v * 219 / 255 and 128 + (v - 128) * 224 / 255, each plus a half for rounding.
    (table(219, 16 * 255 + 127), table(224, 128 * 31 + 127))
}

fn jpeg_layout(
    jpeg: &mut Decompressor,
    frame: &[u8],
    width: usize,
    height: usize,
) -> Result<Layout> {
    let header = jpeg.read_header(frame)?;
    if (header.width, header.height) != (width, height) {
        bail!(
            "the webcam sent {}x{} MJPEG frames instead of {width}x{height}",
            header.width,
            header.height
        );
    }
    Ok(match header.subsamp {
        Subsamp::Sub2x2 => Layout::J420,
        Subsamp::Sub2x1 => Layout::J422,
        Subsamp::None => Layout::J444,
        other => bail!(
            "the webcam's MJPEG uses {other:?} chroma, which is not supported; \
             set [camera] input_format = \"yuyv\""
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder(layout: Layout, width: usize, height: usize) -> Decoder {
        let (luma, chroma) = range_tables();
        Decoder {
            jpeg: Decompressor::new().unwrap(),
            width,
            height,
            layout,
            planes: Vec::new(),
            luma,
            chroma,
            warned: false,
            workers: Vec::new(),
        }
    }

    #[test]
    fn scales_to_limited_range() {
        let d = decoder(Layout::J422, 2, 2);
        assert_eq!((d.luma[0], d.luma[128], d.luma[255]), (16, 126, 235));
        assert_eq!((d.chroma[0], d.chroma[128], d.chroma[255]), (16, 128, 240));
    }

    #[test]
    fn packs_planar_layouts() {
        let d = decoder(Layout::J422, 4, 2);
        let mut planes = [[0, 255, 0, 255], [255, 0, 255, 0]].concat();
        planes.extend([128, 0, 128, 255, 255, 0, 0, 128]);
        let mut out = [0; 16];
        d.pack_planar(&planes, &mut out);
        assert_eq!(&out[..8], &[16, 128, 235, 240, 16, 16, 235, 16]);

        let d = decoder(Layout::J444, 2, 1);
        let mut out = [0; 4];
        d.pack_planar(&[255, 255, 100, 102, 128, 128], &mut out);
        assert_eq!(out, [235, d.chroma[101], 235, 128]);

        let d = decoder(Layout::J420, 2, 2);
        let mut out = [0; 8];
        d.pack_planar(&[0, 0, 255, 255, 255, 0], &mut out);
        assert_eq!(out, [16, 240, 16, 16, 235, 240, 235, 16]);
    }

    #[test]
    fn packs_nv12() {
        let mut d = decoder(Layout::Nv12, 2, 2);
        let frame = [1, 2, 3, 4, 50, 60];
        let mut out = [0; 8];
        assert!(d.fill_for_loopback(&frame, &mut out).unwrap());
        assert_eq!(out, [1, 50, 2, 60, 3, 50, 4, 60]);
        assert!(!d.fill_for_loopback(&frame[..5], &mut out).unwrap());
    }
}
