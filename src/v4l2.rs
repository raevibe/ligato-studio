//! Minimal, dependency-free V4L2 bindings (x86_64 / aarch64 Linux layouts).
//!
//! Only the ioctls the sc0710 driver implements and this app needs are bound.
//! Struct sizes are asserted in the tests at the bottom of the file because
//! the ioctl request numbers encode them.
#![allow(dead_code)] // binding surface is wider than the app's current use

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

pub const PIX_YUYV: u32 = fourcc(b'Y', b'U', b'Y', b'V');
pub const PIX_BGR24: u32 = fourcc(b'B', b'G', b'R', b'3');
pub const PIX_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');

pub fn fourcc_str(f: u32) -> String {
    f.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect()
}

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
pub const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
pub const EVENT_SOURCE_CHANGE: u32 = 5;
pub const EVENT_CTRL: u32 = 3;
const CTRL_FLAG_NEXT_CTRL: u32 = 0x8000_0000;
pub const CTRL_TYPE_MENU: u32 = 3;

// ---- ioctl number encoding -------------------------------------------------
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}
const fn ior<T>(nr: u64) -> u64 { ioc(IOC_READ, nr, std::mem::size_of::<T>()) }
const fn iow<T>(nr: u64) -> u64 { ioc(IOC_WRITE, nr, std::mem::size_of::<T>()) }
const fn iowr<T>(nr: u64) -> u64 { ioc(IOC_READ | IOC_WRITE, nr, std::mem::size_of::<T>()) }

// ---- kernel structs ----------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FmtDesc {
    pub index: u32,
    pub type_: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PixFormat {
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

#[repr(C)]
#[derive(Clone, Copy)]
union FormatUnion {
    pix: PixFormat,
    raw: [u8; 200],
    _align: [u64; 25],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Format {
    type_: u32,
    fmt: FormatUnion,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
union BufferM {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut libc::c_void,
    fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Buffer {
    pub index: u32,
    type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: libc::timeval,
    timecode: Timecode,
    pub sequence: u32,
    memory: u32,
    m: BufferM,
    pub length: u32,
    reserved2: u32,
    request_fd: i32,
}

impl Buffer {
    fn new(index: u32) -> Self {
        // SAFETY: all-zero is a valid bit pattern for this plain C struct.
        let mut b: Buffer = unsafe { std::mem::zeroed() };
        b.index = index;
        b.type_ = BUF_TYPE_VIDEO_CAPTURE;
        b.memory = MEMORY_MMAP;
        b
    }
    pub fn timestamp_us(&self) -> i64 {
        self.timestamp.tv_sec * 1_000_000 + self.timestamp.tv_usec
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Fract {
    numerator: u32,
    denominator: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CaptureParm {
    capability: u32,
    capturemode: u32,
    timeperframe: Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
union StreamParmUnion {
    capture: CaptureParm,
    raw: [u8; 200],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StreamParm {
    type_: u32,
    parm: StreamParmUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct QueryCtrl {
    pub id: u32,
    pub type_: u32,
    pub name: [u8; 32],
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub default_value: i32,
    pub flags: u32,
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Control {
    id: u32,
    value: i32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct QueryMenu {
    id: u32,
    index: u32,
    name: [u8; 32],
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Edid {
    pad: u32,
    start_block: u32,
    blocks: u32,
    reserved: [u32; 5],
    edid: *mut u8,
}

/// `struct v4l2_dv_timings` is packed: u32 type + 128-byte union.
#[repr(C)]
#[derive(Clone, Copy)]
struct DvTimingsRaw {
    type_: u32,
    data: [u8; 128],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EventSubscription {
    type_: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
union EventUnion {
    data: [u8; 64],
    _align: [u64; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Event {
    type_: u32,
    u: EventUnion,
    pending: u32,
    sequence: u32,
    timestamp: libc::timespec,
    id: u32,
    reserved: [u32; 8],
}

const VIDIOC_QUERYCAP: u64 = ior::<Capability>(0);
const VIDIOC_ENUM_FMT: u64 = iowr::<FmtDesc>(2);
const VIDIOC_G_FMT: u64 = iowr::<Format>(4);
const VIDIOC_S_FMT: u64 = iowr::<Format>(5);
const VIDIOC_REQBUFS: u64 = iowr::<RequestBuffers>(8);
const VIDIOC_QUERYBUF: u64 = iowr::<Buffer>(9);
const VIDIOC_QBUF: u64 = iowr::<Buffer>(15);
const VIDIOC_DQBUF: u64 = iowr::<Buffer>(17);
const VIDIOC_STREAMON: u64 = iow::<libc::c_int>(18);
const VIDIOC_STREAMOFF: u64 = iow::<libc::c_int>(19);
const VIDIOC_G_PARM: u64 = iowr::<StreamParm>(21);
const VIDIOC_G_CTRL: u64 = iowr::<Control>(27);
const VIDIOC_S_CTRL: u64 = iowr::<Control>(28);
const VIDIOC_QUERYCTRL: u64 = iowr::<QueryCtrl>(36);
const VIDIOC_QUERYMENU: u64 = iowr::<QueryMenu>(37);
const VIDIOC_G_EDID: u64 = iowr::<Edid>(40);
const VIDIOC_S_EDID: u64 = iowr::<Edid>(41);
const VIDIOC_G_DV_TIMINGS: u64 = iowr::<DvTimingsRaw>(88);
const VIDIOC_DQEVENT: u64 = ior::<Event>(89);
const VIDIOC_SUBSCRIBE_EVENT: u64 = iow::<EventSubscription>(90);
const VIDIOC_QUERY_DV_TIMINGS: u64 = ior::<DvTimingsRaw>(99);

fn xioctl<T>(fd: RawFd, req: u64, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `arg` is a properly sized repr(C) struct matching `req`.
        let r = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
        if r != -1 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

fn cstr(b: &[u8]) -> String {
    CStr::from_bytes_until_nul(b)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| String::from_utf8_lossy(b).into_owned())
}

// ---- safe wrapper ----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CapInfo {
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    pub device_caps: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DvTimings {
    pub width: u32,
    pub height: u32,
    pub interlaced: bool,
    pub pixelclock: u64,
    pub htotal: u32,
    pub vtotal: u32,
}

impl DvTimings {
    pub fn fps(&self) -> f64 {
        let total = self.htotal as f64 * self.vtotal as f64;
        if total <= 0.0 || self.pixelclock == 0 {
            return 0.0;
        }
        let f = self.pixelclock as f64 / total;
        if self.interlaced { f * 2.0 } else { f }
    }
}

#[derive(Debug, Clone)]
pub struct CtrlInfo {
    pub id: u32,
    pub type_: u32,
    pub name: String,
    pub minimum: i32,
    pub maximum: i32,
    pub default_value: i32,
    pub menu: Vec<(i32, String)>,
}

pub struct Device {
    file: File,
}

impl Device {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        Ok(Self { file })
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn querycap(&self) -> io::Result<CapInfo> {
        // SAFETY: zeroed plain C struct.
        let mut c: Capability = unsafe { std::mem::zeroed() };
        xioctl(self.fd(), VIDIOC_QUERYCAP, &mut c)?;
        let caps = if c.capabilities & CAP_DEVICE_CAPS != 0 { c.device_caps } else { c.capabilities };
        Ok(CapInfo {
            driver: cstr(&c.driver),
            card: cstr(&c.card),
            bus_info: cstr(&c.bus_info),
            device_caps: caps,
        })
    }

    pub fn enum_formats(&self) -> Vec<(u32, String)> {
        let mut out = Vec::new();
        for index in 0..32 {
            // SAFETY: zeroed plain C struct.
            let mut d: FmtDesc = unsafe { std::mem::zeroed() };
            d.index = index;
            d.type_ = BUF_TYPE_VIDEO_CAPTURE;
            if xioctl(self.fd(), VIDIOC_ENUM_FMT, &mut d).is_err() {
                break;
            }
            out.push((d.pixelformat, cstr(&d.description)));
        }
        out
    }

    pub fn get_format(&self) -> io::Result<PixFormat> {
        // SAFETY: zeroed plain C struct/union.
        let mut f: Format = unsafe { std::mem::zeroed() };
        f.type_ = BUF_TYPE_VIDEO_CAPTURE;
        xioctl(self.fd(), VIDIOC_G_FMT, &mut f)?;
        // SAFETY: the driver filled the `pix` member for a capture type.
        Ok(unsafe { f.fmt.pix })
    }

    /// Requests a pixel format; the driver clamps size to the live signal.
    pub fn set_format(&self, pixelformat: u32, width: u32, height: u32) -> io::Result<PixFormat> {
        // SAFETY: zeroed plain C struct/union.
        let mut f: Format = unsafe { std::mem::zeroed() };
        f.type_ = BUF_TYPE_VIDEO_CAPTURE;
        f.fmt.pix = PixFormat { width, height, pixelformat, ..Default::default() };
        xioctl(self.fd(), VIDIOC_S_FMT, &mut f)?;
        // SAFETY: as above.
        Ok(unsafe { f.fmt.pix })
    }

    pub fn frame_interval(&self) -> Option<(u32, u32)> {
        // SAFETY: zeroed plain C struct/union.
        let mut p: StreamParm = unsafe { std::mem::zeroed() };
        p.type_ = BUF_TYPE_VIDEO_CAPTURE;
        xioctl(self.fd(), VIDIOC_G_PARM, &mut p).ok()?;
        // SAFETY: capture member is valid for a capture type.
        let t = unsafe { p.parm.capture.timeperframe };
        (t.numerator != 0 && t.denominator != 0).then_some((t.numerator, t.denominator))
    }

    fn dv(&self, req: u64) -> io::Result<DvTimings> {
        let mut t = DvTimingsRaw { type_: 0, data: [0; 128] };
        xioctl(self.fd(), req, &mut t)?;
        let d = &t.data;
        let r32 = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let width = r32(0);
        let height = r32(4);
        // bt.hfrontporch/hsync/hbackporch at 24.., v* at 36.. (packed struct)
        let (hfp, hs, hbp) = (r32(24), r32(28), r32(32));
        let (vfp, vs, vbp) = (r32(36), r32(40), r32(44));
        Ok(DvTimings {
            width,
            height,
            interlaced: r32(8) != 0,
            pixelclock: u64::from_le_bytes(d[16..24].try_into().unwrap()),
            htotal: width + hfp + hs + hbp,
            vtotal: height + vfp + vs + vbp,
        })
    }

    /// Live signal timings. `ENOLINK`/`ENOLCK` mean no signal.
    pub fn query_dv_timings(&self) -> io::Result<DvTimings> {
        self.dv(VIDIOC_QUERY_DV_TIMINGS)
    }

    pub fn get_dv_timings(&self) -> io::Result<DvTimings> {
        self.dv(VIDIOC_G_DV_TIMINGS)
    }

    pub fn controls(&self) -> Vec<CtrlInfo> {
        let mut out = Vec::new();
        let mut id = CTRL_FLAG_NEXT_CTRL;
        loop {
            // SAFETY: zeroed plain C struct.
            let mut q: QueryCtrl = unsafe { std::mem::zeroed() };
            q.id = id;
            if xioctl(self.fd(), VIDIOC_QUERYCTRL, &mut q).is_err() {
                break;
            }
            let mut info = CtrlInfo {
                id: q.id,
                type_: q.type_,
                name: cstr(&q.name),
                minimum: q.minimum,
                maximum: q.maximum,
                default_value: q.default_value,
                menu: Vec::new(),
            };
            if q.type_ == CTRL_TYPE_MENU {
                for i in q.minimum..=q.maximum {
                    let mut m = QueryMenu { id: q.id, index: i as u32, name: [0; 32], reserved: 0 };
                    if xioctl(self.fd(), VIDIOC_QUERYMENU, &mut m).is_ok() {
                        let name = m.name;
                        info.menu.push((i, cstr(&name)));
                    }
                }
            }
            out.push(info);
            id = q.id | CTRL_FLAG_NEXT_CTRL;
        }
        out
    }

    pub fn get_ctrl(&self, id: u32) -> io::Result<i32> {
        let mut c = Control { id, value: 0 };
        xioctl(self.fd(), VIDIOC_G_CTRL, &mut c)?;
        Ok(c.value)
    }

    pub fn set_ctrl(&self, id: u32, value: i32) -> io::Result<()> {
        let mut c = Control { id, value };
        xioctl(self.fd(), VIDIOC_S_CTRL, &mut c)
    }

    pub fn get_edid(&self) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; 128 * 4];
        let mut e = Edid { pad: 0, start_block: 0, blocks: 4, reserved: [0; 5], edid: buf.as_mut_ptr() };
        xioctl(self.fd(), VIDIOC_G_EDID, &mut e)?;
        buf.truncate(e.blocks as usize * 128);
        Ok(buf)
    }

    pub fn set_edid(&self, edid: &[u8]) -> io::Result<()> {
        if edid.is_empty() || !edid.len().is_multiple_of(128) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "EDID must be a multiple of 128 bytes"));
        }
        let mut buf = edid.to_vec();
        let mut e = Edid {
            pad: 0,
            start_block: 0,
            blocks: (buf.len() / 128) as u32,
            reserved: [0; 5],
            edid: buf.as_mut_ptr(),
        };
        xioctl(self.fd(), VIDIOC_S_EDID, &mut e)
    }

    pub fn subscribe_event(&self, type_: u32, id: u32) -> io::Result<()> {
        let mut s = EventSubscription { type_, id, ..Default::default() };
        xioctl(self.fd(), VIDIOC_SUBSCRIBE_EVENT, &mut s)
    }

    /// Returns the type of the next pending event, if any.
    pub fn dequeue_event(&self) -> Option<u32> {
        // SAFETY: zeroed plain C struct/union.
        let mut e: Event = unsafe { std::mem::zeroed() };
        xioctl(self.fd(), VIDIOC_DQEVENT, &mut e).ok().map(|_| e.type_)
    }
}

// ---- mmap streaming ----------------------------------------------------------

pub struct MappedBuffer {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is owned exclusively by the Stream and only read.
unsafe impl Send for MappedBuffer {}

impl Drop for MappedBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap.
        unsafe { libc::munmap(self.ptr as *mut _, self.len) };
    }
}

pub struct Stream<'a> {
    dev: &'a Device,
    buffers: Vec<MappedBuffer>,
    streaming: bool,
}

pub struct Frame<'s> {
    pub data: &'s [u8],
    pub sequence: u32,
    pub timestamp_us: i64,
    index: u32,
}

impl<'a> Stream<'a> {
    pub fn new(dev: &'a Device, count: u32) -> io::Result<Self> {
        let mut req = RequestBuffers { count, type_: BUF_TYPE_VIDEO_CAPTURE, memory: MEMORY_MMAP, ..Default::default() };
        xioctl(dev.fd(), VIDIOC_REQBUFS, &mut req)?;
        if req.count == 0 {
            return Err(io::Error::other("driver granted no buffers"));
        }
        let mut s = Stream { dev, buffers: Vec::new(), streaming: false };
        for i in 0..req.count {
            let mut b = Buffer::new(i);
            xioctl(dev.fd(), VIDIOC_QUERYBUF, &mut b)?;
            // SAFETY: offset/length come from QUERYBUF for this fd.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    b.length as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    dev.fd(),
                    b.m.offset as libc::off_t,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            s.buffers.push(MappedBuffer { ptr: ptr as *mut u8, len: b.length as usize });
        }
        for i in 0..req.count {
            let mut b = Buffer::new(i);
            xioctl(dev.fd(), VIDIOC_QBUF, &mut b)?;
        }
        let mut t: libc::c_int = BUF_TYPE_VIDEO_CAPTURE as _;
        xioctl(dev.fd(), VIDIOC_STREAMON, &mut t)?;
        s.streaming = true;
        Ok(s)
    }

    /// Waits up to `timeout_ms` for a frame or event.
    /// Returns (frame ready, event pending).
    pub fn poll(&self, timeout_ms: i32) -> io::Result<(bool, bool)> {
        let mut p = libc::pollfd { fd: self.dev.fd(), events: libc::POLLIN | libc::POLLPRI, revents: 0 };
        // SAFETY: single valid pollfd.
        let r = unsafe { libc::poll(&mut p, 1, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok((false, false));
            }
            return Err(e);
        }
        if p.revents & libc::POLLERR != 0 {
            return Err(io::Error::other("device error (POLLERR)"));
        }
        Ok((p.revents & libc::POLLIN != 0, p.revents & libc::POLLPRI != 0))
    }

    pub fn dequeue(&self) -> io::Result<Option<Frame<'_>>> {
        let mut b = Buffer::new(0);
        match xioctl(self.dev.fd(), VIDIOC_DQBUF, &mut b) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(None),
            Err(e) => return Err(e),
        }
        let mb = &self.buffers[b.index as usize];
        let used = (b.bytesused as usize).min(mb.len);
        // SAFETY: the buffer is owned by userspace until re-queued.
        let data = unsafe { std::slice::from_raw_parts(mb.ptr, used) };
        Ok(Some(Frame { data, sequence: b.sequence, timestamp_us: b.timestamp_us(), index: b.index }))
    }

    pub fn requeue(&self, frame: Frame<'_>) -> io::Result<()> {
        let mut b = Buffer::new(frame.index);
        xioctl(self.dev.fd(), VIDIOC_QBUF, &mut b)
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        if self.streaming {
            let mut t: libc::c_int = BUF_TYPE_VIDEO_CAPTURE as _;
            let _ = xioctl(self.dev.fd(), VIDIOC_STREAMOFF, &mut t);
        }
        self.buffers.clear();
        let mut req = RequestBuffers { count: 0, type_: BUF_TYPE_VIDEO_CAPTURE, memory: MEMORY_MMAP, ..Default::default() };
        let _ = xioctl(self.dev.fd(), VIDIOC_REQBUFS, &mut req);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn struct_sizes_match_kernel_abi() {
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<FmtDesc>(), 64);
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(size_of::<RequestBuffers>(), 20);
        assert_eq!(size_of::<Buffer>(), 88);
        assert_eq!(size_of::<StreamParm>(), 204);
        assert_eq!(size_of::<QueryCtrl>(), 68);
        assert_eq!(size_of::<QueryMenu>(), 44);
        assert_eq!(size_of::<Edid>(), 40);
        assert_eq!(size_of::<DvTimingsRaw>(), 132);
        assert_eq!(size_of::<EventSubscription>(), 32);
        assert_eq!(size_of::<Event>(), 136);
    }

    #[test]
    fn ioctl_numbers() {
        // Reference values from <linux/videodev2.h> on x86_64.
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_QUERY_DV_TIMINGS, 0x8084_5663);
        assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
        assert_eq!(VIDIOC_S_EDID, 0xc028_5629);
    }
}
