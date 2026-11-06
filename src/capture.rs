//! Capture thread: owns the V4L2 stream, feeds the preview, the recorder and
//! snapshots, and reconfigures itself on source changes.

use crate::convert::{FrameFormat, Processor};
use crate::recorder::{RecordConfig, Recorder};
use crate::settings::{ColorRange, Picture, PixelFormatPref};
use crate::v4l2::{self, Device, Stream, PIX_BGR24, PIX_NV12, PIX_YUYV};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub enum Command {
    SetPixelFormat(PixelFormatPref),
    SetProcessing(ColorRange, Picture),
    StartRecording(RecordConfig, bool),
    StopRecording,
    Snapshot(PathBuf),
    Shutdown,
}

#[derive(Default, Clone)]
pub struct Preview {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub serial: u64,
}

#[derive(Default, Clone)]
pub struct RecordingStatus {
    pub path: PathBuf,
    pub started: Option<Instant>,
    pub written: u64,
    pub dropped: u64,
}

#[derive(Default, Clone)]
pub struct Status {
    pub format: Option<FrameFormat>,
    pub interval: Option<(u32, u32)>,
    pub measured_fps: f32,
    pub frames: u64,
    pub streaming: bool,
    pub error: Option<String>,
    pub recording: Option<RecordingStatus>,
    /// One-shot messages for the UI (snapshot saved, recording finished…).
    pub messages: Vec<String>,
    pub full_range: bool,
}

pub struct Shared {
    pub preview: Mutex<Preview>,
    pub status: Mutex<Status>,
}

pub struct CaptureHandle {
    tx: Sender<Command>,
    pub shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    pub path: PathBuf,
}

impl CaptureHandle {
    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct StartOptions {
    pub pixel_format: PixelFormatPref,
    pub range: ColorRange,
    pub picture: Picture,
    pub preview_width: u32,
    pub repaint: Box<dyn Fn() + Send>,
}

pub fn start(path: PathBuf, opts: StartOptions) -> CaptureHandle {
    let (tx, rx) = channel();
    let shared = Arc::new(Shared { preview: Mutex::new(Preview::default()), status: Mutex::new(Status::default()) });
    let sh = shared.clone();
    let p = path.clone();
    let thread = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || Worker::new(p, opts, rx, sh).run())
        .expect("spawn capture thread");
    CaptureHandle { tx, shared, thread: Some(thread), path }
}

struct Worker {
    path: PathBuf,
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    pixel_format: PixelFormatPref,
    range: ColorRange,
    picture: Picture,
    processor: Processor,
    preview_width: u32,
    repaint: Box<dyn Fn() + Send>,
    recorder: Option<Recorder>,
    pending_record: Option<(RecordConfig, bool)>,
    snapshot: Option<PathBuf>,
    serial: u64,
    shutdown: bool,
}

enum Outcome {
    Reconfigure,
    Shutdown,
}

impl Worker {
    fn new(path: PathBuf, o: StartOptions, rx: Receiver<Command>, shared: Arc<Shared>) -> Worker {
        Worker {
            path,
            rx,
            shared,
            pixel_format: o.pixel_format,
            range: o.range,
            picture: o.picture,
            processor: Processor::new(o.range, o.picture),
            preview_width: o.preview_width,
            repaint: o.repaint,
            recorder: None,
            pending_record: None,
            snapshot: None,
            serial: 0,
            shutdown: false,
        }
    }

    fn set_error(&self, e: Option<String>) {
        let mut s = self.shared.status.lock().unwrap();
        s.error = e;
        s.streaming = false;
        drop(s);
        (self.repaint)();
    }

    fn message(&self, m: String) {
        log::info!("{m}");
        self.shared.status.lock().unwrap().messages.push(m);
        (self.repaint)();
    }

    fn run(mut self) {
        while !self.shutdown {
            let dev = match Device::open(&self.path) {
                Ok(d) => d,
                Err(e) => {
                    self.set_error(Some(format!("cannot open {}: {e}", self.path.display())));
                    if self.wait_commands(Duration::from_secs(1)) {
                        break;
                    }
                    continue;
                }
            };
            let _ = dev.subscribe_event(v4l2::EVENT_SOURCE_CHANGE, 0);
            loop {
                match self.stream_once(&dev) {
                    Ok(Outcome::Reconfigure) => continue,
                    Ok(Outcome::Shutdown) => {
                        self.shutdown = true;
                        break;
                    }
                    Err(e) => {
                        let gone = e.raw_os_error() == Some(libc::ENODEV);
                        self.set_error(Some(format!("capture error: {e}")));
                        if self.wait_commands(Duration::from_millis(700)) {
                            self.shutdown = true;
                            break;
                        }
                        if gone {
                            break;
                        }
                    }
                }
            }
        }
        self.finish_recording("capture stopped");
    }

    /// Handles commands for `d` while not streaming. Returns true on shutdown.
    fn wait_commands(&mut self, d: Duration) -> bool {
        let end = Instant::now() + d;
        while Instant::now() < end {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(c) => {
                    if self.handle(c, None) {
                        return true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return true,
            }
        }
        false
    }

    /// Returns (shutdown, needs_reconfigure).
    fn handle(&mut self, c: Command, fmt: Option<&FrameFormat>) -> bool {
        match c {
            Command::SetPixelFormat(p) => {
                if p != self.pixel_format {
                    self.pixel_format = p;
                    if self.recorder.is_some() {
                        self.message("Pixel format change will apply after recording stops".into());
                    }
                }
            }
            Command::SetProcessing(r, p) => {
                self.range = r;
                self.picture = p;
                self.processor = Processor::new(r, p);
                self.shared.status.lock().unwrap().full_range = self.full_range(fmt);
            }
            Command::StartRecording(cfg, hdr) => {
                if self.recorder.is_none() {
                    self.pending_record = Some((cfg, hdr));
                }
            }
            Command::StopRecording => {
                self.pending_record = None;
                self.finish_recording("stopped");
            }
            Command::Snapshot(p) => self.snapshot = Some(p),
            Command::Shutdown => return true,
        }
        false
    }

    fn full_range(&self, fmt: Option<&FrameFormat>) -> bool {
        match self.range {
            ColorRange::Expand => true,
            ColorRange::Shrink => false,
            // RGB from the card is full-range sRGB; YUV is limited by default.
            ColorRange::Bypass => fmt.map(|f| f.fourcc == PIX_BGR24).unwrap_or(false),
        }
    }

    fn finish_recording(&mut self, why: &str) {
        if let Some(r) = self.recorder.take() {
            let dropped = r.dropped.load(Ordering::Relaxed);
            match r.stop() {
                Ok(p) => self.message(format!("Recording {why}: saved {} ({dropped} frames dropped)", p.display())),
                Err(e) => self.message(format!("Recording {why} with an error: {e:#}")),
            }
        }
        self.shared.status.lock().unwrap().recording = None;
    }

    fn desired_fourcc(&self, dev: &Device) -> u32 {
        let fmts: Vec<u32> = dev.enum_formats().into_iter().map(|f| f.0).collect();
        let want = match self.pixel_format {
            PixelFormatPref::Yuyv => PIX_YUYV,
            PixelFormatPref::Bgr24 => PIX_BGR24,
        };
        if fmts.contains(&want) {
            want
        } else if let Some(f) = [PIX_YUYV, PIX_NV12, PIX_BGR24].into_iter().find(|f| fmts.contains(f)) {
            f
        } else {
            want
        }
    }

    fn stream_once(&mut self, dev: &Device) -> std::io::Result<Outcome> {
        let cur = dev.get_format()?;
        let want = self.desired_fourcc(dev);
        let pix = if cur.pixelformat != want {
            match dev.set_format(want, cur.width, cur.height) {
                Ok(p) => p,
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    self.message("Pixel format is in use by another app; keeping the current one".into());
                    cur
                }
                Err(e) => return Err(e),
            }
        } else {
            // Re-apply to pick up a size change after a source change.
            dev.set_format(cur.pixelformat, cur.width, cur.height).unwrap_or(cur)
        };
        let fmt = FrameFormat { fourcc: pix.pixelformat, width: pix.width, height: pix.height, stride: pix.bytesperline };
        let interval = dev.frame_interval();
        let full = self.full_range(Some(&fmt));
        {
            let mut s = self.shared.status.lock().unwrap();
            s.format = Some(fmt);
            s.interval = interval;
            s.error = if fmt.supported() { None } else { Some(format!("unsupported pixel format {}", v4l2::fourcc_str(fmt.fourcc))) };
            s.full_range = full;
        }
        let stream = Stream::new(dev, 4)?;
        self.shared.status.lock().unwrap().streaming = true;

        // A recording that was active across a format change is not resumed
        // automatically; a pending start uses the fresh format.
        if let Some(r) = &self.recorder {
            if r.format != fmt {
                self.finish_recording("stopped because the input format changed");
            }
        }

        let fps = interval.map(|(n, d)| (d, n)).unwrap_or((60, 1));
        let mut rec_timeline: Option<(i64, i64)> = None;
        let mut fps_count = 0u32;
        let mut fps_t = Instant::now();
        let mut last_preview = Instant::now() - Duration::from_secs(1);
        let mut last_fmt_check = Instant::now();
        let mut packed: Vec<u8> = Vec::new();
        let pixel_pref_at_start = self.pixel_format;

        loop {
            loop {
                match self.rx.try_recv() {
                    Ok(c) => {
                        if self.handle(c, Some(&fmt)) {
                            return Ok(Outcome::Shutdown);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(Outcome::Shutdown),
                }
            }
            if self.pixel_format != pixel_pref_at_start && self.recorder.is_none() {
                return Ok(Outcome::Reconfigure);
            }
            if let Some((cfg, hdr)) = self.pending_record.take() {
                if fmt.supported() {
                    match Recorder::start(&cfg, fmt, fps, self.full_range(Some(&fmt)), hdr) {
                        Ok(r) => {
                            self.shared.status.lock().unwrap().recording = Some(RecordingStatus {
                                path: r.path.clone(),
                                started: Some(r.started),
                                ..Default::default()
                            });
                            self.recorder = Some(r);
                            rec_timeline = None;
                        }
                        Err(e) => self.message(format!("Could not start recording: {e:#}")),
                    }
                } else {
                    self.message("Cannot record: no supported video format".into());
                }
            }
            if let Some(r) = &mut self.recorder {
                if let Some(err) = r.check() {
                    self.recorder = None;
                    self.shared.status.lock().unwrap().recording = None;
                    self.message(err);
                }
            }

            let (ready, event) = stream.poll(100)?;
            if event {
                let mut changed = false;
                while let Some(t) = dev.dequeue_event() {
                    changed |= t == v4l2::EVENT_SOURCE_CHANGE;
                }
                if changed {
                    log::info!("source change event");
                    return Ok(Outcome::Reconfigure);
                }
            }
            if last_fmt_check.elapsed() > Duration::from_secs(1) {
                last_fmt_check = Instant::now();
                if let Ok(now) = dev.get_format() {
                    if now.width != fmt.width || now.height != fmt.height || now.pixelformat != fmt.fourcc {
                        return Ok(Outcome::Reconfigure);
                    }
                }
            }
            if !ready {
                continue;
            }
            let Some(frame) = stream.dequeue()? else { continue };
            fps_count += 1;
            if fps_t.elapsed() >= Duration::from_secs(1) {
                let mut s = self.shared.status.lock().unwrap();
                s.measured_fps = fps_count as f32 / fps_t.elapsed().as_secs_f32();
                s.frames += fps_count as u64;
                if let (Some(rs), Some(r)) = (s.recording.as_mut(), self.recorder.as_ref()) {
                    rs.written = r.written.load(Ordering::Relaxed);
                    rs.dropped = r.dropped.load(Ordering::Relaxed);
                }
                drop(s);
                fps_count = 0;
                fps_t = Instant::now();
                (self.repaint)();
            }

            if let Some(rec) = &self.recorder {
                // Place each frame on the constant-rate output timeline by its
                // capture timestamp: duplicate across gaps, drop when the
                // source runs faster than the nominal rate. This keeps video
                // aligned with the live audio clock.
                let (t0, last_idx) = *rec_timeline.get_or_insert((frame.timestamp_us, -1));
                let idx = ((frame.timestamp_us - t0) as f64 * fps.0 as f64 / fps.1 as f64 / 1e6).round() as i64;
                let repeat = (idx - last_idx).clamp(0, 8) as u32;
                rec_timeline = Some((t0, idx.max(last_idx)));
                if repeat > 0 && fmt.pack(frame.data, &mut packed) {
                    self.processor.process(&fmt, &mut packed);
                    rec.push(Arc::new(std::mem::take(&mut packed)), repeat);
                }
            }

            if let Some(p) = self.snapshot.take() {
                let mut buf = Vec::new();
                if fmt.pack(frame.data, &mut buf) {
                    let tight = FrameFormat { stride: fmt.tight_stride(), ..fmt };
                    self.processor.process(&tight, &mut buf);
                    let proc_id = Processor::new(ColorRange::Bypass, Picture::default());
                    let full = self.full_range(Some(&fmt));
                    let shared = self.shared.clone();
                    std::thread::spawn(move || {
                        let msg = match proc_id.preview(&tight, &buf, tight.width, full) {
                            Some((w, h, rgba)) => match save_png(&p, w, h, &rgba) {
                                Ok(()) => format!("Snapshot saved: {}", p.display()),
                                Err(e) => format!("Snapshot failed: {e}"),
                            },
                            None => "Snapshot failed: unsupported format".into(),
                        };
                        shared.status.lock().unwrap().messages.push(msg);
                    });
                }
            }

            // Preview at most ~60 Hz.
            if last_preview.elapsed() >= Duration::from_millis(15) {
                last_preview = Instant::now();
                let full = self.full_range(Some(&fmt));
                if let Some((w, h, rgba)) = self.processor.preview(&fmt, frame.data, self.preview_width, full) {
                    self.serial += 1;
                    *self.shared.preview.lock().unwrap() = Preview { width: w, height: h, rgba, serial: self.serial };
                    (self.repaint)();
                }
            }
            stream.requeue(frame)?;
        }
    }
}

fn save_png(path: &Path, w: usize, h: usize, rgba: &[u8]) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(f, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}
