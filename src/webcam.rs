use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{InputColor, InputFormat};
use crate::v4l2::{self, PixFormat};

const VIDIOC_QUERYCAP: libc::c_ulong = 0x8068_5600;
const VIDIOC_G_FMT: libc::c_ulong = 0xC0D0_5604;
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
const CAP_TIMEPERFRAME: u32 = 0x1000;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub matrix: Matrix,
    pub full_range: bool,
}

impl Color {
    pub const LIMITED_601: Self = Self {
        matrix: Matrix::Bt601,
        full_range: false,
    };
    /// JFIF: always BT.601 full range, whatever the driver reports.
    pub const JPEG: Self = Self {
        matrix: Matrix::Bt601,
        full_range: true,
    };

    /// Resolves defaults like the kernel's `V4L2_MAP_*_DEFAULT` macros.
    fn reported(pix: &PixFormat) -> Self {
        const COLORSPACE_SMPTE240M: u32 = 2;
        const COLORSPACE_REC709: u32 = 3;
        const COLORSPACE_JPEG: u32 = 7;
        const COLORSPACE_BT2020: u32 = 10;
        const COLORSPACE_DCI_P3: u32 = 12;
        const YCBCR_ENC_DEFAULT: u32 = 0;
        const YCBCR_ENC_709: u32 = 2;
        const YCBCR_ENC_XV709: u32 = 4;
        const YCBCR_ENC_BT2020: u32 = 6;
        const YCBCR_ENC_BT2020_CONST_LUM: u32 = 7;
        const YCBCR_ENC_SMPTE240M: u32 = 8;
        const QUANTIZATION_FULL_RANGE: u32 = 1;
        const QUANTIZATION_LIM_RANGE: u32 = 2;
        // NvCV lacks SMPTE 240M, which is close to BT.709.
        let matrix = match (pix.ycbcr_enc, pix.colorspace) {
            (YCBCR_ENC_709 | YCBCR_ENC_XV709 | YCBCR_ENC_SMPTE240M, _)
            | (YCBCR_ENC_DEFAULT, COLORSPACE_REC709 | COLORSPACE_DCI_P3 | COLORSPACE_SMPTE240M) => {
                Matrix::Bt709
            }
            (YCBCR_ENC_BT2020 | YCBCR_ENC_BT2020_CONST_LUM, _)
            | (YCBCR_ENC_DEFAULT, COLORSPACE_BT2020) => Matrix::Bt2020,
            _ => Matrix::Bt601,
        };
        let full_range = match pix.quantization {
            QUANTIZATION_FULL_RANGE => true,
            QUANTIZATION_LIM_RANGE => false,
            _ => pix.colorspace == COLORSPACE_JPEG,
        };
        Self { matrix, full_range }
    }

    pub fn resolve(format: Format, setting: InputColor, reported: Self) -> Self {
        let (matrix, full_range) = match (format, setting) {
            (Format::Mjpeg, _) => return Self::JPEG,
            (_, InputColor::Auto) => return reported,
            (_, InputColor::Bt601) => (Matrix::Bt601, false),
            (_, InputColor::Bt601Full) => (Matrix::Bt601, true),
            (_, InputColor::Bt709) => (Matrix::Bt709, false),
            (_, InputColor::Bt709Full) => (Matrix::Bt709, true),
        };
        Self { matrix, full_range }
    }

    pub fn worker_name(self) -> &'static str {
        match (self.matrix, self.full_range) {
            (Matrix::Bt601, false) => "601",
            (Matrix::Bt601, true) => "601-full",
            (Matrix::Bt709, false) => "709",
            (Matrix::Bt709, true) => "709-full",
            (Matrix::Bt2020, false) => "2020",
            (Matrix::Bt2020, true) => "2020-full",
        }
    }
}

impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let matrix = match self.matrix {
            Matrix::Bt601 => "BT.601",
            Matrix::Bt709 => "BT.709",
            Matrix::Bt2020 => "BT.2020",
        };
        let range = if self.full_range { "full" } else { "limited" };
        write!(f, "{matrix} {range} range")
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

pub struct Webcam {
    pub path: String,
    pub name: String,
    /// In any supported format, largest first.
    pub sizes: Vec<(u32, u32)>,
    formats: Vec<(Format, Sizes)>,
}

impl Webcam {
    /// The format the service would capture in, if any.
    pub fn format_for(&self, wanted: InputFormat, width: u32, height: u32) -> Option<Format> {
        find(&self.formats, wanted, width, height)
    }
}

/// Sizes offered when a webcam reports a range instead of a list.
const COMMON_SIZES: [(u32, u32); 6] = [
    (3840, 2160),
    (2560, 1440),
    (1920, 1080),
    (1280, 720),
    (960, 540),
    (640, 480),
];

/// Skips v4l2loopback devices and webcams without a supported format.
pub fn list() -> Vec<Webcam> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(v4l2::VIDEO_CLASS)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| !v4l2::is_loopback_dir(dir))
        .collect();
    dirs.sort_by_key(|dir| {
        dir.file_name()
            .and_then(|name| name.to_str()?.strip_prefix("video")?.parse::<u32>().ok())
    });
    dirs.iter()
        .filter_map(|dir| {
            let path = format!("/dev/{}", dir.file_name()?.to_str()?);
            let formats = list_formats(&open(&path).ok()?);
            let mut sizes: Vec<(u32, u32)> = formats
                .iter()
                .flat_map(|(_, sizes)| match sizes {
                    Sizes::Discrete(list) => list.clone(),
                    Sizes::Range(_) => COMMON_SIZES
                        .into_iter()
                        .filter(|&(w, h)| sizes.fits(w, h))
                        .collect(),
                })
                .collect();
            if sizes.is_empty() {
                return None;
            }
            sizes.sort_by_key(|&(w, h)| std::cmp::Reverse(u64::from(w) * u64::from(h)));
            sizes.dedup();
            let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
            Some(Webcam {
                path,
                name: name.trim().to_owned(),
                sizes,
                formats,
            })
        })
        .collect()
}

/// Lets a reader open the loopback at the size the service writes.
pub fn capture_size(path: &str) -> Result<(u32, u32)> {
    let file = open(path)?;
    // SAFETY: v4l2_pix_format is plain integers; all-zero is a valid value.
    let mut format = v4l2::Format::new(BUF_TYPE_VIDEO_CAPTURE, unsafe { std::mem::zeroed() });
    ioctl(&file, VIDIOC_G_FMT, &mut format)
        .with_context(|| format!("reading the format of {path}"))?;
    Ok((format.pix.width, format.pix.height))
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
    if let Some(format) = find(formats, wanted, width, height) {
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

fn find(
    formats: &[(Format, Sizes)],
    wanted: InputFormat,
    width: u32,
    height: u32,
) -> Option<Format> {
    Format::candidates(wanted).iter().copied().find(|c| {
        formats
            .iter()
            .any(|(f, sizes)| f == c && sizes.fits(width, height))
    })
}

fn set_fps(file: &File, input: &str, format: Format, (width, height): (u32, u32), fps: u32) {
    let mut parm = StreamParm {
        type_: BUF_TYPE_VIDEO_CAPTURE,
        parm: [0; 50],
    };
    parm.parm[2] = 1;
    parm.parm[3] = fps;
    if let Err(e) = ioctl(file, VIDIOC_S_PARM, &mut parm) {
        eprintln!("camera: {input} did not accept {fps} fps: {e}");
        return;
    }
    if parm.parm[0] & CAP_TIMEPERFRAME == 0 {
        return;
    }
    if let Some(actual) = slower_fps(parm.parm[2], parm.parm[3], fps) {
        let mjpeg = format != Format::Mjpeg
            && find(&list_formats(file), InputFormat::Mjpeg, width, height).is_some();
        let hint = if mjpeg {
            "MJPEG or a smaller size may be faster"
        } else {
            "a smaller size may be faster"
        };
        eprintln!(
            "camera: {input} gives only {actual} fps as {} at {width}x{height}, not {fps}; {hint}",
            format.label()
        );
    }
}

/// The frame rate the driver set, as text, when it is under 90% of `wanted`.
fn slower_fps(numerator: u32, denominator: u32, wanted: u32) -> Option<String> {
    if numerator == 0
        || denominator == 0
        || u64::from(denominator) * 10 >= u64::from(wanted) * u64::from(numerator) * 9
    {
        return None;
    }
    let tenths = (u64::from(denominator) * 10 + u64::from(numerator) / 2) / u64::from(numerator);
    Some(if tenths % 10 == 0 {
        format!("{}", tenths / 10)
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    })
}

struct Mapping {
    ptr: NonNull<libc::c_void>,
    len: usize,
}

pub struct Capture {
    file: File,
    buffers: Vec<Mapping>,
    color: Color,
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

        set_fps(&file, input, format, (width, height), fps);

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
            color: Color::reported(&pix),
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

    pub fn reported_color(&self) -> Color {
        self.color
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

    fn pix(colorspace: u32, ycbcr_enc: u32, quantization: u32) -> PixFormat {
        PixFormat {
            width: 0,
            height: 0,
            pixelformat: 0,
            field: 0,
            bytesperline: 0,
            sizeimage: 0,
            colorspace,
            priv_: 0,
            flags: 0,
            ycbcr_enc,
            quantization,
            xfer_func: 0,
        }
    }

    #[test]
    fn resolves_reported_colors() {
        let color = |matrix, full_range| Color { matrix, full_range };
        assert_eq!(Color::reported(&pix(8, 1, 0)), Color::LIMITED_601);
        assert_eq!(Color::reported(&pix(8, 0, 0)), Color::LIMITED_601);
        assert_eq!(Color::reported(&pix(3, 0, 0)), color(Matrix::Bt709, false));
        assert_eq!(Color::reported(&pix(8, 2, 1)), color(Matrix::Bt709, true));
        assert_eq!(
            Color::reported(&pix(10, 0, 0)),
            color(Matrix::Bt2020, false)
        );
        assert_eq!(Color::reported(&pix(2, 0, 0)).matrix, Matrix::Bt709);
        assert_eq!(Color::reported(&pix(7, 0, 0)), Color::JPEG);
        assert_eq!(Color::reported(&pix(7, 1, 2)), Color::LIMITED_601);
    }

    #[test]
    fn setting_overrides_raw_formats_only() {
        let bt709 = Color {
            matrix: Matrix::Bt709,
            full_range: false,
        };
        assert_eq!(Color::resolve(Format::Yuyv, InputColor::Auto, bt709), bt709);
        assert_eq!(
            Color::resolve(Format::Nv12, InputColor::Bt601Full, bt709),
            Color::JPEG
        );
        assert_eq!(
            Color::resolve(Format::Mjpeg, InputColor::Auto, bt709),
            Color::JPEG
        );
        assert_eq!(
            Color::resolve(Format::Mjpeg, InputColor::Bt709, Color::LIMITED_601),
            Color::JPEG
        );
        assert_eq!(bt709.to_string(), "BT.709 limited range");
        assert_eq!(Color::JPEG.worker_name(), "601-full");
    }

    #[test]
    fn reports_slower_frame_rates() {
        assert_eq!(slower_fps(1, 5, 30).as_deref(), Some("5"));
        assert_eq!(slower_fps(2, 15, 30).as_deref(), Some("7.5"));
        assert_eq!(slower_fps(1001, 30000, 30), None);
        assert_eq!(slower_fps(1, 30, 30), None);
        assert_eq!(slower_fps(1, 60, 30), None);
        assert_eq!(slower_fps(0, 0, 30), None);
        assert_eq!(slower_fps(1, 0, 30), None);
    }

    #[test]
    fn finds_formats_at_a_size() {
        let webcam = Webcam {
            path: "/dev/video0".into(),
            name: "BRIO".into(),
            sizes: vec![(1920, 1080)],
            formats: brio(),
        };
        let at_1080p = |wanted| webcam.format_for(wanted, 1920, 1080);
        assert_eq!(at_1080p(InputFormat::Auto), Some(Format::Mjpeg));
        assert_eq!(at_1080p(InputFormat::Yuyv), Some(Format::Yuyv));
        assert_eq!(at_1080p(InputFormat::Nv12), None);
        assert_eq!(
            webcam.format_for(InputFormat::Auto, 340, 340),
            Some(Format::Yuyv)
        );
        assert_eq!(webcam.format_for(InputFormat::Mjpeg, 340, 340), None);
    }
}
