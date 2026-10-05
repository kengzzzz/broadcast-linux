use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

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
            .map_err(|e| anyhow!("opening {path}: {e}{}", open_hint(&e)))?;
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
            let e = io::Error::last_os_error();
            if !is_loopback(path) {
                bail!(
                    "{path} is not a v4l2loopback device ({e}); {}",
                    devices_hint()
                );
            }
            bail!("setting the output format on {path}: {e}");
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

const VIDEO_CLASS: &str = "/sys/class/video4linux";

/// Checks the loopback device without opening it; returns its label.
pub(crate) fn check(path: &str) -> Result<String> {
    if !is_loopback(path) {
        if Path::new(path).exists() {
            bail!("{path} is not a v4l2loopback device; {}", devices_hint());
        }
        bail!("{path} does not exist; {}", devices_hint());
    }
    let c_path = std::ffi::CString::new(path)?;
    // SAFETY: access() only reads the NUL-terminated path.
    if unsafe { libc::access(c_path.as_ptr(), libc::R_OK | libc::W_OK) } != 0 {
        let e = io::Error::last_os_error();
        bail!("{path}: {e}{}", open_hint(&e));
    }
    Ok(fs::canonicalize(path)
        .ok()
        .and_then(|dev| {
            fs::read_to_string(Path::new(VIDEO_CLASS).join(dev.file_name()?).join("name")).ok()
        })
        .unwrap_or_default()
        .trim()
        .to_owned())
}

fn open_hint(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => format!("; {}", devices_hint()),
        io::ErrorKind::PermissionDenied => {
            "; join the video group (`sudo usermod -aG video $USER`), then log in again".into()
        }
        _ => String::new(),
    }
}

/// `max_openers` is a sysfs attribute only v4l2loopback devices have.
fn is_loopback_dir(dir: &Path) -> bool {
    dir.join("max_openers").exists()
}

fn is_loopback(path: &str) -> bool {
    fs::canonicalize(path)
        .ok()
        .and_then(|dev| {
            dev.file_name()
                .map(|name| Path::new(VIDEO_CLASS).join(name))
        })
        .is_some_and(|dir| is_loopback_dir(&dir))
}

fn devices_hint() -> String {
    if !Path::new("/sys/module/v4l2loopback").exists() {
        return "v4l2loopback is not loaded: run `sudo modprobe v4l2loopback`".into();
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(VIDEO_CLASS)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| is_loopback_dir(dir))
        .collect();
    if dirs.is_empty() {
        return "no v4l2loopback devices exist; reload the module to apply its modprobe.d \
                options: `sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback`"
            .into();
    }
    dirs.sort_by_key(|dir| {
        dir.file_name()
            .and_then(|name| name.to_str()?.strip_prefix("video")?.parse::<u32>().ok())
    });
    let devices: Vec<String> = dirs
        .iter()
        .filter_map(|dir| {
            let node = dir.file_name()?.to_string_lossy();
            let label = fs::read_to_string(dir.join("name")).unwrap_or_default();
            Some(format!("/dev/{node} \"{}\"", label.trim()))
        })
        .collect();
    format!(
        "set [camera] device to a v4l2loopback device: {}",
        devices.join(", ")
    )
}

/// Converts packed BGR24 to YUYV (BT.601, limited range), two pixels at a time.
#[allow(clippy::many_single_char_names)]
pub fn bgr_to_yuyv(bgr: &[u8], yuyv: &mut [u8]) {
    for (src, dst) in bgr
        .as_chunks::<6>()
        .0
        .iter()
        .zip(yuyv.as_chunks_mut::<4>().0)
    {
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
        *dst = [
            clamp(y(r0, g0, b0)),
            clamp(u),
            clamp(y(r1, g1, b1)),
            clamp(v),
        ];
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
