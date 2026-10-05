use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::ptr::NonNull;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::InputFormat;
use crate::v4l2::{self, PixFormat};

const VIDIOC_QUERYCAP: libc::c_ulong = 0x8068_5600;
const VIDIOC_ENUM_FMT: libc::c_ulong = 0xC040_5602;
const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong = 0xC02C_564A;
const VIDIOC_S_PARM: libc::c_ulong = 0xC0CC_5616;
const VIDIOC_REQBUFS: libc::c_ulong = 0xC014_5608;
const VIDIOC_QUERYBUF: libc::c_ulong = 0xC058_5609;
const VIDIOC_QBUF: libc::c_ulong = 0xC058_560F;
const VIDIOC_DQBUF: libc::c_ulong = 0xC058_5611;
const VIDIOC_STREAMON: libc::c_ulong = 0x4004_5612;
const VIDIOC_STREAMOFF: libc::c_ulong = 0x4004_5613;
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_NONE: u32 = 1;
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const BUF_FLAG_ERROR: u32 = 0x0000_0040;
const FRMSIZE_TYPE_DISCRETE: u32 = 1;
const BUFFERS: u32 = 4;

#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct FmtDesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_frmsizeenum`: `size` is width and height when discrete, else the
/// minimum, maximum and step of the width, then of the height.
#[repr(C)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    type_: u32,
    size: [u32; 6],
    reserved: [u32; 2],
}

/// `struct v4l2_streamparm` for capture: `parm[2..4]` is timeperframe.
#[repr(C)]
struct StreamParm {
    type_: u32,
    parm: [u32; 50],
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

/// `struct v4l2_buffer` on 64-bit: `offset` is the mmap member of union `m`.
#[repr(C)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    _pad: u32,
    timestamp: [i64; 2],
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    offset: u64,
    length: u32,
    reserved2: u32,
    request_fd: i32,
    _pad2: u32,
}

impl Buffer {
    fn mmap(index: u32) -> Self {
        // SAFETY: v4l2_buffer is plain integers; all-zero is a valid value.
        let mut buf: Self = unsafe { std::mem::zeroed() };
        buf.index = index;
        buf.type_ = BUF_TYPE_VIDEO_CAPTURE;
        buf.memory = MEMORY_MMAP;
        buf
    }
}

fn ioctl<T>(file: &File, request: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: every request in this file is paired with the struct the kernel
        // expects for it; the struct sizes are checked in the tests below.
        if unsafe { libc::ioctl(file.as_raw_fd(), request, std::ptr::from_mut(arg)) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mjpeg,
    Yuyv,
    Nv12,
}

impl Format {
    /// Some webcams list motion JPEG as `JPEG` rather than `MJPG`.
    fn fourccs(self) -> &'static [u32] {
        const MJPG: u32 = u32::from_le_bytes(*b"MJPG");
        const JPEG: u32 = u32::from_le_bytes(*b"JPEG");
        const NV12: u32 = u32::from_le_bytes(*b"NV12");
        match self {
            Self::Mjpeg => &[MJPG, JPEG],
            Self::Yuyv => &[v4l2::YUYV],
            Self::Nv12 => &[NV12],
        }
    }

    fn from_fourcc(fourcc: u32) -> Option<Self> {
        [Self::Mjpeg, Self::Yuyv, Self::Nv12]
            .into_iter()
            .find(|format| format.fourccs().contains(&fourcc))
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mjpeg => "MJPEG",
            Self::Yuyv => "YUYV",
            Self::Nv12 => "NV12",
        }
    }

    fn candidates(wanted: InputFormat) -> &'static [Self] {
        match wanted {
            InputFormat::Auto => &[Self::Mjpeg, Self::Yuyv, Self::Nv12],
            InputFormat::Mjpeg => &[Self::Mjpeg],
            InputFormat::Yuyv => &[Self::Yuyv],
            InputFormat::Nv12 => &[Self::Nv12],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Sizes {
    Discrete(Vec<(u32, u32)>),
    /// (min, max, step) for width, then height.
    Range([u32; 6]),
}

impl Sizes {
    fn fits(&self, width: u32, height: u32) -> bool {
        let within = |v: u32, min: u32, max: u32, step: u32| {
            (min..=max).contains(&v) && (v - min).is_multiple_of(step.max(1))
        };
        match self {
            Self::Discrete(sizes) => sizes.contains(&(width, height)),
            Self::Range([min_w, max_w, step_w, min_h, max_h, step_h]) => {
                within(width, *min_w, *max_w, *step_w) && within(height, *min_h, *max_h, *step_h)
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Discrete(sizes) => sizes
                .iter()
                .map(|(w, h)| format!("{w}x{h}"))
                .collect::<Vec<_>>()
                .join(" "),
            Self::Range([min_w, max_w, _, min_h, max_h, _]) => {
                format!("{min_w}x{min_h} to {max_w}x{max_h}")
            }
        }
    }
}

fn open(input: &str) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(input)
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => anyhow!("{input} does not exist"),
            io::ErrorKind::PermissionDenied => {
                anyhow!("{input}: permission denied; join the video group, then log in again")
            }
            _ => anyhow!("opening {input}: {e}"),
        })?;
    // SAFETY: v4l2_capability is plain bytes and integers; all-zero is a valid value.
    let mut cap: Capability = unsafe { std::mem::zeroed() };
    ioctl(&file, VIDIOC_QUERYCAP, &mut cap)
        .map_err(|e| anyhow!("{input} is not a video device ({e})"))?;
    let caps = if cap.capabilities & CAP_DEVICE_CAPS == 0 {
        cap.capabilities
    } else {
        cap.device_caps
    };
    if caps & (CAP_VIDEO_CAPTURE | CAP_STREAMING) != CAP_VIDEO_CAPTURE | CAP_STREAMING {
        bail!("{input} does not capture video; webcams often have a second node for metadata");
    }
    Ok(file)
}

fn list_formats(file: &File) -> Vec<(Format, Sizes)> {
    let mut formats = Vec::new();
    for index in 0.. {
        // SAFETY: v4l2_fmtdesc is plain bytes and integers; all-zero is a valid value.
        let mut desc: FmtDesc = unsafe { std::mem::zeroed() };
        desc.index = index;
        desc.type_ = BUF_TYPE_VIDEO_CAPTURE;
        if ioctl(file, VIDIOC_ENUM_FMT, &mut desc).is_err() {
            break;
        }
        if let Some(format) = Format::from_fourcc(desc.pixelformat)
            && !formats.iter().any(|(f, _)| *f == format)
        {
            formats.push((format, list_sizes(file, desc.pixelformat)));
        }
    }
    formats
}

fn list_sizes(file: &File, fourcc: u32) -> Sizes {
    let mut sizes = Vec::new();
    for index in 0.. {
        let mut size = FrmSizeEnum {
            index,
            pixel_format: fourcc,
            type_: 0,
            size: [0; 6],
            reserved: [0; 2],
        };
        if ioctl(file, VIDIOC_ENUM_FRAMESIZES, &mut size).is_err() {
            break;
        }
        if size.type_ != FRMSIZE_TYPE_DISCRETE {
            return Sizes::Range(size.size);
        }
        sizes.push((size.size[0], size.size[1]));
    }
    Sizes::Discrete(sizes)
}

/// Picks the capture format for the webcam at `width`x`height`.
pub fn choose(input: &str, wanted: InputFormat, width: u32, height: u32) -> Result<Format> {
    let formats = list_formats(&open(input)?);
    if formats.is_empty() {
        bail!("{input} offers none of MJPEG, YUYV or NV12");
    }
    pick(&formats, wanted, width, height).with_context(|| input.to_owned())
}

fn pick(
    formats: &[(Format, Sizes)],
    wanted: InputFormat,
    width: u32,
    height: u32,
) -> Result<Format> {
    if let Some(format) = Format::candidates(wanted).iter().copied().find(|c| {
        formats
            .iter()
            .any(|(f, sizes)| f == c && sizes.fits(width, height))
    }) {
        return Ok(format);
    }
    let listed: Vec<String> = formats
        .iter()
        .map(|(format, sizes)| format!("{}: {}", format.label(), sizes.describe()))
        .collect();
    bail!(
        "no {} capture at {width}x{height}; set [camera] width/height or input_format to one of: {}",
        wanted.label(),
        listed.join("; ")
    )
}

struct Mapping {
    ptr: NonNull<libc::c_void>,
    len: usize,
}

pub struct Capture {
    file: File,
    buffers: Vec<Mapping>,
}

// SAFETY: the mappings belong to this Capture alone and are only touched through &mut self.
unsafe impl Send for Capture {}

impl Capture {
    pub fn open(input: &str, format: Format, width: u32, height: u32, fps: u32) -> Result<Self> {
        let file = open(input)?;
        let mut accepted = None;
        for &fourcc in format.fourccs() {
            let mut fmt = v4l2::Format::new(
                BUF_TYPE_VIDEO_CAPTURE,
                PixFormat {
                    width,
                    height,
                    pixelformat: fourcc,
                    field: FIELD_NONE,
                    bytesperline: 0,
                    sizeimage: 0,
                    colorspace: 0,
                    priv_: 0,
                    flags: 0,
                    ycbcr_enc: 0,
                    quantization: 0,
                    xfer_func: 0,
                },
            );
            ioctl(&file, v4l2::VIDIOC_S_FMT, &mut fmt).map_err(|e| busy_or(input, &e))?;
            let matches =
                (fmt.pix.width, fmt.pix.height, fmt.pix.pixelformat) == (width, height, fourcc);
            accepted = Some((fmt.pix, matches));
            if matches {
                break;
            }
        }
        let pix = match accepted {
            Some((pix, true)) => pix,
            other => {
                let (w, h) = other.map_or((0, 0), |(pix, _)| (pix.width, pix.height));
                bail!(
                    "{input} switched to {w}x{h} instead of {width}x{height} {}",
                    format.label()
                );
            }
        };
        let packed = match format {
            Format::Mjpeg => true,
            Format::Yuyv => pix.bytesperline == width * 2,
            Format::Nv12 => pix.bytesperline == width,
        };
        if !packed {
            bail!(
                "{input} pads {} rows to {} bytes, which is not supported; try another input_format",
                format.label(),
                pix.bytesperline
            );
        }

        let mut parm = StreamParm {
            type_: BUF_TYPE_VIDEO_CAPTURE,
            parm: [0; 50],
        };
        parm.parm[2] = 1;
        parm.parm[3] = fps;
        if let Err(e) = ioctl(&file, VIDIOC_S_PARM, &mut parm) {
            eprintln!("camera: {input} did not accept {fps} fps: {e}");
        }

        let mut request = RequestBuffers {
            count: BUFFERS,
            type_: BUF_TYPE_VIDEO_CAPTURE,
            memory: MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        ioctl(&file, VIDIOC_REQBUFS, &mut request).map_err(|e| busy_or(input, &e))?;
        let mut capture = Self {
            file,
            buffers: Vec::new(),
        };
        for index in 0..request.count {
            let mut buf = Buffer::mmap(index);
            ioctl(&capture.file, VIDIOC_QUERYBUF, &mut buf).context("querying a webcam buffer")?;
            let len = buf.length as usize;
            // SAFETY: maps the driver buffer at the offset it reported; unmapped in Drop.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    capture.file.as_raw_fd(),
                    libc::off_t::try_from(buf.offset)?,
                )
            };
            if ptr == libc::MAP_FAILED {
                bail!("mapping a webcam buffer: {}", io::Error::last_os_error());
            }
            capture.buffers.push(Mapping {
                ptr: NonNull::new(ptr).context("webcam buffer mapped at null")?,
                len,
            });
            ioctl(&capture.file, VIDIOC_QBUF, &mut buf).context("queueing a webcam buffer")?;
        }
        let mut kind = BUF_TYPE_VIDEO_CAPTURE;
        ioctl(&capture.file, VIDIOC_STREAMON, &mut kind).map_err(|e| busy_or(input, &e))?;
        Ok(capture)
    }

    /// Waits up to `timeout` for a frame and calls `f` with the newest one; frames
    /// that queued up while the caller was busy are dropped rather than replayed.
    pub fn newest<R>(
        &mut self,
        timeout: Duration,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let mut fds = [libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let timeout = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: `fds` is a valid array of pollfd for the duration of the call.
        if unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout) } <= 0 {
            return Ok(None);
        }
        let mut newest: Option<Buffer> = None;
        loop {
            let mut buf = Buffer::mmap(0);
            match ioctl(&self.file, VIDIOC_DQBUF, &mut buf) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e).context("reading the webcam"),
            }
            if buf.flags & BUF_FLAG_ERROR != 0 || buf.bytesused == 0 {
                self.requeue(buf)?;
            } else if let Some(older) = newest.replace(buf) {
                self.requeue(older)?;
            }
        }
        let Some(buf) = newest else {
            return Ok(None);
        };
        let mapping = &self.buffers[buf.index as usize];
        let len = (buf.bytesused as usize).min(mapping.len);
        // SAFETY: the driver filled `len` bytes of this mapping and does not touch it
        // until the buffer is queued again below.
        let frame = unsafe { std::slice::from_raw_parts(mapping.ptr.as_ptr().cast::<u8>(), len) };
        let result = f(frame);
        self.requeue(buf)?;
        Ok(Some(result))
    }

    fn requeue(&self, mut buf: Buffer) -> Result<()> {
        ioctl(&self.file, VIDIOC_QBUF, &mut buf).context("requeueing a webcam buffer")
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let mut kind = BUF_TYPE_VIDEO_CAPTURE;
        let _ = ioctl(&self.file, VIDIOC_STREAMOFF, &mut kind);
        for mapping in &self.buffers {
            // SAFETY: unmaps a region mapped in open(); no slices into it outlive newest().
            unsafe { libc::munmap(mapping.ptr.as_ptr(), mapping.len) };
        }
    }
}

fn busy_or(input: &str, e: &io::Error) -> anyhow::Error {
    if e.raw_os_error() == Some(libc::EBUSY) {
        anyhow!("{input} is in use by another app")
    } else {
        anyhow!("setting up {input}: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brio() -> Vec<(Format, Sizes)> {
        vec![
            (
                Format::Yuyv,
                Sizes::Discrete(vec![(640, 480), (1280, 720), (1920, 1080), (340, 340)]),
            ),
            (
                Format::Mjpeg,
                Sizes::Discrete(vec![(640, 480), (1280, 720), (1920, 1080)]),
            ),
        ]
    }

    #[test]
    fn picks_formats() {
        let formats = brio();
        assert_eq!(
            pick(&formats, InputFormat::Auto, 1920, 1080).unwrap(),
            Format::Mjpeg
        );
        assert_eq!(
            pick(&formats, InputFormat::Auto, 340, 340).unwrap(),
            Format::Yuyv
        );
        assert_eq!(
            pick(&formats, InputFormat::Yuyv, 1920, 1080).unwrap(),
            Format::Yuyv
        );
        assert!(pick(&formats, InputFormat::Nv12, 1920, 1080).is_err());
        let err = pick(&formats, InputFormat::Auto, 1234, 567).unwrap_err();
        assert!(
            err.to_string()
                .contains("MJPEG: 640x480 1280x720 1920x1080")
        );
        let capture_card = vec![(Format::Nv12, Sizes::Range([32, 4096, 2, 32, 2160, 2]))];
        assert_eq!(
            pick(&capture_card, InputFormat::Auto, 1920, 1080).unwrap(),
            Format::Nv12
        );
        assert!(pick(&capture_card, InputFormat::Auto, 1921, 1080).is_err());
    }

    #[test]
    fn reads_fourccs() {
        assert_eq!(Format::from_fourcc(0x4750_4A4D), Some(Format::Mjpeg));
        assert_eq!(Format::from_fourcc(0x5659_5559), Some(Format::Yuyv));
        assert_eq!(Format::from_fourcc(0x3231_564E), Some(Format::Nv12));
        assert_eq!(Format::from_fourcc(u32::from_le_bytes(*b"H264")), None);
        assert_eq!(
            Format::from_fourcc(u32::from_le_bytes(*b"JPEG")),
            Some(Format::Mjpeg)
        );
        for format in [Format::Mjpeg, Format::Yuyv, Format::Nv12] {
            for &fourcc in format.fourccs() {
                assert_eq!(Format::from_fourcc(fourcc), Some(format));
            }
        }
    }

    #[test]
    fn struct_sizes_match_the_kernel() {
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<FmtDesc>(), 64);
        assert_eq!(size_of::<FrmSizeEnum>(), 44);
        assert_eq!(size_of::<StreamParm>(), 204);
        assert_eq!(size_of::<RequestBuffers>(), 20);
        assert_eq!(size_of::<Buffer>(), 88);
        assert_eq!(std::mem::offset_of!(Buffer, offset), 64);
        assert_eq!(std::mem::offset_of!(Buffer, length), 72);
    }
}
