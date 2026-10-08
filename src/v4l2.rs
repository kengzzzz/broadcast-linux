use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::NonNull;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use crate::paths;

pub(crate) const VIDIOC_S_FMT: libc::c_ulong = 0xC0D0_5605;
const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = 0x4020_565A;
const VIDIOC_DQEVENT: libc::c_ulong = 0x8088_5659;
const VIDIOC_REQBUFS: libc::c_ulong = 0xC014_5608;
const VIDIOC_QUERYBUF: libc::c_ulong = 0xC058_5609;
const VIDIOC_QBUF: libc::c_ulong = 0xC058_560F;
const VIDIOC_STREAMON: libc::c_ulong = 0x4004_5612;
const MEMORY_MMAP: u32 = 1;
/// One on screen plus two in progress, so the frame on screen is never overwritten.
const BUFFERS: u32 = 3;
const EVENT_SUB_FL_SEND_INITIAL: u32 = 1;
/// v4l2loopback's private event: 1 while some app streams from the device.
const EVENT_CLIENT_USAGE: u32 = 0x0800_0000 + 0x08E0_0000 + 1;
const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const FIELD_NONE: u32 = 1;
const COLORSPACE_SRGB: u32 = 8;
pub(crate) const YUYV: u32 = u32::from_le_bytes(*b"YUYV");

#[repr(C)]
pub(crate) struct PixFormat {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub priv_: u32,
    pub flags: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
}

/// `struct v4l2_format`: the union starts 8-byte aligned and is 200 bytes.
#[repr(C)]
pub(crate) struct Format {
    pub type_: u32,
    _align: u32,
    pub pix: PixFormat,
    _rest: [u8; 200 - size_of::<PixFormat>()],
}

impl Format {
    pub fn new(type_: u32, pix: PixFormat) -> Self {
        Self {
            type_,
            _align: 0,
            pix,
            _rest: [0; 200 - size_of::<PixFormat>()],
        }
    }
}

#[repr(C)]
struct EventSubscription {
    type_: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

#[repr(C)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct v4l2_buffer`; `m` is a union whose first member is the mmap offset.
#[repr(C)]
#[derive(Default)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: [i64; 2],
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    m: [u32; 2],
    length: u32,
    reserved2: u32,
    request_fd: u32,
}

/// The loopback's own frame buffers, mapped so frames can be made in place.
pub struct Buffers {
    maps: Vec<NonNull<u8>>,
    offsets: Vec<u32>,
    frame_bytes: usize,
    path: String,
}

// SAFETY: buffers are only reached through camera slots, which the token protocol gives to
// one thread or process at a time, or by write_frame() while no session runs.
unsafe impl Send for Buffers {}
// SAFETY: as above.
unsafe impl Sync for Buffers {}

impl Buffers {
    pub fn count(&self) -> usize {
        self.maps.len()
    }

    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Reopening this path gives another descriptor that can map the same buffers.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// # Safety
    /// Nothing else may use buffer `index` while the slice lives.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn frame(&self, index: usize) -> &mut [u8] {
        // SAFETY: each map is `frame_bytes` long; the caller makes this the only reference.
        unsafe { std::slice::from_raw_parts_mut(self.maps[index].as_ptr(), self.frame_bytes) }
    }
}

impl Drop for Buffers {
    fn drop(&mut self) {
        for map in &self.maps {
            // SAFETY: unmaps a region mapped in map_buffers(); no slices outlive self.
            unsafe { libc::munmap(map.as_ptr().cast(), self.frame_bytes) };
        }
    }
}

pub struct Loopback {
    file: File,
    buffers: Option<Arc<Buffers>>,
    shown: usize,
}

impl Loopback {
    /// Maps the device's buffers when the driver allows it, and falls back to `write()`.
    pub fn open(path: &str, width: u32, height: u32) -> Result<Self> {
        let file = open_output(path, width, height)?;
        let frame_bytes = width as usize * height as usize * 2;
        let (file, buffers) = match map_buffers(&file, frame_bytes) {
            Ok(buffers) => (file, Some(Arc::new(buffers))),
            Err(e) => {
                eprintln!("camera: copying frames into {path}: {e}");
                // A failed attempt can leave the descriptor in streaming mode, where
                // write() is refused.
                drop(file);
                (open_output(path, width, height)?, None)
            }
        };
        let loopback = Self {
            file,
            buffers,
            shown: 0,
        };
        // The initial event reports readers that were already streaming, e.g. across a
        // service restart.
        let mut sub = EventSubscription {
            type_: EVENT_CLIENT_USAGE,
            id: 0,
            flags: EVENT_SUB_FL_SEND_INITIAL,
            reserved: [0; 5],
        };
        // SAFETY: `sub` matches struct v4l2_event_subscription (32 bytes).
        if unsafe { libc::ioctl(loopback.fd(), VIDIOC_SUBSCRIBE_EVENT, &raw mut sub) } < 0 {
            bail!(
                "{path} does not report readers (needs v4l2loopback 0.12.6+): {}",
                io::Error::last_os_error()
            );
        }
        Ok(loopback)
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn buffers(&self) -> Option<Arc<Buffers>> {
        self.buffers.clone()
    }

    /// The buffer queued last, which stays on screen until another is queued.
    pub fn shown(&self) -> usize {
        self.shown
    }

    /// # Safety
    /// With mapped buffers, nothing else may be writing any of them.
    pub unsafe fn write_frame(&mut self, yuyv: &[u8]) -> io::Result<()> {
        let Some(buffers) = &self.buffers else {
            return self.file.write_all(yuyv);
        };
        let index = (self.shown + 1) % buffers.count();
        // SAFETY: the caller guarantees nothing else writes the buffers.
        unsafe { buffers.frame(index) }.copy_from_slice(yuyv);
        self.queue(index)
    }

    pub fn queue(&mut self, index: usize) -> io::Result<()> {
        let Some(buffers) = &self.buffers else {
            return Err(io::ErrorKind::Unsupported.into());
        };
        let mut buffer = Buffer {
            index: u32::try_from(index).map_err(io::Error::other)?,
            type_: BUF_TYPE_VIDEO_OUTPUT,
            bytesused: u32::try_from(buffers.frame_bytes).map_err(io::Error::other)?,
            memory: MEMORY_MMAP,
            ..Buffer::default()
        };
        // SAFETY: `buffer` matches struct v4l2_buffer (88 bytes).
        if unsafe { libc::ioctl(self.fd(), VIDIOC_QBUF, &raw mut buffer) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.shown = index;
        Ok(())
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

fn open_output(path: &str, width: u32, height: u32) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| match open_hint(&e) {
            Some(hint) => anyhow!("opening {path}: {e}; {hint}"),
            None => anyhow!("opening {path}: {e}"),
        })?;
    let mut format = Format::new(
        BUF_TYPE_VIDEO_OUTPUT,
        PixFormat {
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
    );
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
    // While another app holds the device, the driver keeps its format and reports
    // success anyway; frames of the requested size would then be garbled.
    let (w, h) = (format.pix.width, format.pix.height);
    if (w, h) != (width, height) || format.pix.pixelformat != YUYV {
        bail!(
            "{path} is still in use at {w}x{h}; close the apps using the camera, then restart the service"
        );
    }
    Ok(file)
}

fn map_buffers(file: &File, frame_bytes: usize) -> Result<Buffers> {
    // Before 0.14 the driver folds buffer numbers onto however many buffers a reader
    // last asked for, so a queued buffer may not be the one shown.
    let version = fs::read_to_string("/sys/module/v4l2loopback/version")
        .context("reading the v4l2loopback version")?;
    let version = version.trim();
    let mut parts = version.split('.').map(|n| n.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if (major, minor) < (0, 14) {
        bail!("v4l2loopback {version} is older than 0.14");
    }
    let fd = file.as_raw_fd();
    let mut request = RequestBuffers {
        count: BUFFERS,
        type_: BUF_TYPE_VIDEO_OUTPUT,
        memory: MEMORY_MMAP,
        capabilities: 0,
        flags: 0,
        reserved: [0; 3],
    };
    // SAFETY: `request` matches struct v4l2_requestbuffers (20 bytes).
    if unsafe { libc::ioctl(fd, VIDIOC_REQBUFS, &raw mut request) } < 0 {
        bail!("requesting buffers: {}", io::Error::last_os_error());
    }
    if request.count != BUFFERS {
        bail!(
            "got {} buffers instead of {BUFFERS}; skipping the copy needs v4l2loopback loaded \
             with max_buffers={BUFFERS} or more",
            request.count
        );
    }
    let mut buffers = Buffers {
        maps: Vec::new(),
        offsets: Vec::new(),
        frame_bytes,
        path: format!("/proc/{}/fd/{fd}", std::process::id()),
    };
    for index in 0..BUFFERS {
        let mut buffer = Buffer {
            index,
            type_: BUF_TYPE_VIDEO_OUTPUT,
            memory: MEMORY_MMAP,
            ..Buffer::default()
        };
        // SAFETY: `buffer` matches struct v4l2_buffer (88 bytes).
        if unsafe { libc::ioctl(fd, VIDIOC_QUERYBUF, &raw mut buffer) } < 0 {
            bail!("querying buffer {index}: {}", io::Error::last_os_error());
        }
        if (buffer.length as usize) < frame_bytes {
            bail!("buffer {index} holds {} bytes", buffer.length);
        }
        let offset = buffer.m[0];
        // SAFETY: maps one driver buffer; unmapped when `buffers` drops.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                frame_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                libc::off_t::from(offset),
            )
        };
        if map == libc::MAP_FAILED {
            bail!("mapping buffer {index}: {}", io::Error::last_os_error());
        }
        buffers
            .maps
            .push(NonNull::new(map.cast()).context("buffer mapped at null")?);
        buffers.offsets.push(offset);
    }
    let mut type_ = BUF_TYPE_VIDEO_OUTPUT;
    // SAFETY: STREAMON takes a pointer to the buffer type.
    if unsafe { libc::ioctl(fd, VIDIOC_STREAMON, &raw mut type_) } < 0 {
        bail!("starting the stream: {}", io::Error::last_os_error());
    }
    Ok(buffers)
}

pub(crate) const VIDEO_CLASS: &str = "/sys/class/video4linux";

#[derive(Clone, Debug)]
pub struct Hint {
    pub text: String,
    pub commands: Vec<String>,
}

impl Hint {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            commands: Vec::new(),
        }
    }

    fn prefixed(mut self, prefix: &str) -> Self {
        self.text = format!("{prefix}; {}", self.text);
        self
    }
}

impl fmt::Display for Hint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)?;
        for (i, command) in self.commands.iter().enumerate() {
            let join = if i == 0 { ": run" } else { ", then" };
            write!(f, "{join} `{command}`")?;
        }
        Ok(())
    }
}

impl std::error::Error for Hint {}

/// Doesn't open the device; returns its label.
pub fn check(path: &str) -> Result<String, Hint> {
    if !is_loopback(path) {
        let problem = if Path::new(path).exists() {
            format!("{path} is not a v4l2loopback device")
        } else {
            format!("{path} does not exist")
        };
        return Err(devices_hint().prefixed(&problem));
    }
    let c_path = std::ffi::CString::new(path).map_err(|_| Hint::text("invalid device path"))?;
    // SAFETY: access() only reads the NUL-terminated path.
    if unsafe { libc::access(c_path.as_ptr(), libc::R_OK | libc::W_OK) } != 0 {
        let e = io::Error::last_os_error();
        let problem = format!("{path}: {e}");
        return Err(match open_hint(&e) {
            Some(hint) => hint.prefixed(&problem),
            None => Hint::text(problem),
        });
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

fn open_hint(e: &io::Error) -> Option<Hint> {
    match e.kind() {
        io::ErrorKind::NotFound => Some(devices_hint()),
        io::ErrorKind::PermissionDenied => Some(Hint {
            text: "join the video group, then log in again".into(),
            commands: vec!["sudo usermod -aG video $USER".into()],
        }),
        _ => None,
    }
}

/// `max_openers` is a sysfs attribute only v4l2loopback devices have.
pub(crate) fn is_loopback_dir(dir: &Path) -> bool {
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

fn devices_hint() -> Hint {
    let mut commands = module_config_commands();
    if !Path::new("/sys/module/v4l2loopback").exists() {
        commands.push("sudo modprobe v4l2loopback".into());
        let text = if module_installed() {
            "v4l2loopback is not loaded"
        } else {
            "v4l2loopback is not installed; install your distribution's v4l2loopback \
             package (often v4l2loopback-dkms), then load it"
        };
        return Hint {
            text: text.into(),
            commands,
        };
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(VIDEO_CLASS)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| is_loopback_dir(dir))
        .collect();
    if dirs.is_empty() || !commands.is_empty() {
        commands.push("sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback".into());
        return Hint {
            text: "v4l2loopback was loaded without this app's options; reload it".into(),
            commands,
        };
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
    Hint::text(format!(
        "set [camera] device to a v4l2loopback device: {}",
        devices.join(", ")
    ))
}

/// Installs the module options and autoload files, unless a package or an earlier run did.
fn module_config_commands() -> Vec<String> {
    let share = paths::share_dir();
    [
        ("modprobe.d", "modprobe.conf"),
        ("modules-load.d", "modules-load.conf"),
    ]
    .into_iter()
    .filter(|(dir, _)| {
        ["/etc", "/usr/lib", "/lib"].iter().all(|root| {
            !Path::new(root)
                .join(dir)
                .join("broadcast-linux.conf")
                .exists()
        })
    })
    .map(|(dir, file)| {
        format!(
            "sudo install -Dm644 {} /etc/{dir}/broadcast-linux.conf",
            share.join(file).display()
        )
    })
    .collect()
}

fn module_installed() -> bool {
    Command::new("modinfo")
        .args(["-n", "v4l2loopback"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_the_kernel() {
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(size_of::<EventSubscription>(), 32);
        assert_eq!(size_of::<RequestBuffers>(), 20);
        assert_eq!(size_of::<Buffer>(), 88);
    }
}
