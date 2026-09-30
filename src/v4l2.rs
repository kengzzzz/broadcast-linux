use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, bail};

const VIDIOC_S_FMT: libc::c_ulong = 0xC0D0_5605;
const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = 0x4020_565A;
const VIDIOC_DQEVENT: libc::c_ulong = 0x8088_5659;
/// v4l2loopback's private event: 1 while some app streams from the device.
const EVENT_CLIENT_USAGE: u32 = 0x0800_0000 + 0x08E0_0000 + 1;
const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const FIELD_NONE: u32 = 1;
const COLORSPACE_SRGB: u32 = 8;
const YUYV: u32 = u32::from_le_bytes(*b"YUYV");

#[repr(C)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`: the union starts 8-byte aligned and is 200 bytes.
#[repr(C)]
struct Format {
    type_: u32,
    _align: u32,
    pix: PixFormat,
    _rest: [u8; 200 - size_of::<PixFormat>()],
}

#[repr(C)]
struct EventSubscription {
    type_: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

pub struct Loopback {
    file: File,
}

impl Loopback {
    pub fn open(path: &str, width: u32, height: u32) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("opening {path} (is v4l2loopback loaded?)"))?;
        let mut format = Format {
            type_: BUF_TYPE_VIDEO_OUTPUT,
            _align: 0,
            pix: PixFormat {
                width,
                height,
                pixelformat: YUYV,
                field: FIELD_NONE,
                bytesperline: width * 2,
                sizeimage: width * height * 2,
                colorspace: COLORSPACE_SRGB,
                priv_: 0,
                flags: 0,
                ycbcr_enc: 0,
                quantization: 0,
                xfer_func: 0,
            },
            _rest: [0; 200 - size_of::<PixFormat>()],
        };
        // SAFETY: `format` matches the kernel's struct v4l2_format layout (208 bytes).
        if unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_S_FMT, &raw mut format) } < 0 {
            bail!(
                "setting the output format on {path}: {}",
                io::Error::last_os_error()
            );
        }
        let mut sub = EventSubscription {
            type_: EVENT_CLIENT_USAGE,
            id: 0,
            flags: 0,
            reserved: [0; 5],
        };
        // SAFETY: `sub` matches struct v4l2_event_subscription (32 bytes).
        if unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_SUBSCRIBE_EVENT, &raw mut sub) } < 0 {
            bail!(
                "{path} does not report readers (needs v4l2loopback 0.12.6+): {}",
                io::Error::last_os_error()
            );
        }
        Ok(Self { file })
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn write_frame(&mut self, yuyv: &[u8]) -> io::Result<()> {
        self.file.write_all(yuyv)
    }

    pub fn take_usage(&self) -> Option<bool> {
        let mut latest = None;
        let mut event = [0u8; 136];
        // SAFETY: the buffer is the size of struct v4l2_event; the fd is non-blocking.
        while unsafe { libc::ioctl(self.fd(), VIDIOC_DQEVENT, event.as_mut_ptr()) } == 0 {
            let kind = u32::from_ne_bytes([event[0], event[1], event[2], event[3]]);
            if kind == EVENT_CLIENT_USAGE {
                latest = Some(u32::from_ne_bytes([event[8], event[9], event[10], event[11]]) > 0);
            }
        }
        latest
    }
}

/// Converts packed BGR24 to YUYV (BT.601, limited range), two pixels at a time.
#[allow(clippy::many_single_char_names)]
pub fn bgr_to_yuyv(bgr: &[u8], yuyv: &mut [u8]) {
    for (src, dst) in bgr.chunks_exact(6).zip(yuyv.chunks_exact_mut(4)) {
        let px = |i: usize| {
            (
                i32::from(src[i + 2]),
                i32::from(src[i + 1]),
                i32::from(src[i]),
            )
        };
        let (r0, g0, b0) = px(0);
        let (r1, g1, b1) = px(3);
        let y = |r: i32, g: i32, b: i32| ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
        let (r, g, b) = (r0.midpoint(r1), g0.midpoint(g1), b0.midpoint(b1));
        let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
        let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
        let clamp = |x: i32| u8::try_from(x.clamp(0, 255)).unwrap_or(0);
        dst.copy_from_slice(&[
            clamp(y(r0, g0, b0)),
            clamp(u),
            clamp(y(r1, g1, b1)),
            clamp(v),
        ]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_reference_colours() {
        let mut out = [0u8; 4];
        bgr_to_yuyv(&[0, 0, 0, 0, 0, 0], &mut out);
        assert_eq!(out, [16, 128, 16, 128]);
        bgr_to_yuyv(&[255, 255, 255, 255, 255, 255], &mut out);
        assert_eq!(out, [235, 128, 235, 128]);
        bgr_to_yuyv(&[0, 255, 0, 0, 255, 0], &mut out);
        assert_eq!(out, [144, 54, 144, 34]);
    }

    #[test]
    fn struct_sizes_match_the_kernel() {
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(size_of::<EventSubscription>(), 32);
    }
}
