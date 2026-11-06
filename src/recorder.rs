//! Recording through a `gst-launch-1.0` child process.
//!
//! Raw frames (already processed) are piped into `fdsrc ! rawvideoparse`, so
//! the app keeps sole control of the V4L2 stream and no GStreamer V4L2 plugin
//! or development headers are needed. Audio comes from the card's ALSA PCM.

use crate::convert::FrameFormat;
use crate::settings::Container;
use crate::v4l2::{PIX_BGR24, PIX_NV12, PIX_YUYV};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub fn gst_has(element: &str) -> bool {
    Command::new("gst-inspect-1.0")
        .args(["--exists", element])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct EncoderInfo {
    pub id: &'static str,
    pub label: &'static str,
}

const VIDEO_ENCODERS: &[EncoderInfo] = &[
    EncoderInfo { id: "nvautogpuh265enc", label: "NVIDIA NVENC HEVC" },
    EncoderInfo { id: "nvautogpuh264enc", label: "NVIDIA NVENC H.264" },
    EncoderInfo { id: "nvcudah265enc", label: "NVIDIA NVENC HEVC (CUDA)" },
    EncoderInfo { id: "nvcudah264enc", label: "NVIDIA NVENC H.264 (CUDA)" },
    EncoderInfo { id: "nvh265enc", label: "NVIDIA NVENC HEVC (legacy)" },
    EncoderInfo { id: "nvh264enc", label: "NVIDIA NVENC H.264 (legacy)" },
    EncoderInfo { id: "vah265enc", label: "VA-API HEVC" },
    EncoderInfo { id: "vah264enc", label: "VA-API H.264" },
    EncoderInfo { id: "vaapih264enc", label: "VA-API H.264 (legacy)" },
    EncoderInfo { id: "x264enc", label: "x264 (CPU)" },
    EncoderInfo { id: "openh264enc", label: "OpenH264 (CPU)" },
];

const AUDIO_ENCODERS: &[EncoderInfo] = &[
    EncoderInfo { id: "fdkaacenc", label: "AAC (FDK)" },
    EncoderInfo { id: "avenc_aac", label: "AAC (FFmpeg)" },
    EncoderInfo { id: "voaacenc", label: "AAC (VisualOn)" },
    EncoderInfo { id: "opusenc", label: "Opus (MKV only)" },
    EncoderInfo { id: "lamemp3enc", label: "MP3" },
];

/// Encoders that exist *and* can encode a test frame. Some installs register
/// elements that fail at runtime (e.g. legacy NVENC: "Selected preset not
/// supported"), so existence alone is not enough. Probes run in parallel.
pub fn available_video_encoders() -> Vec<EncoderInfo> {
    let handles: Vec<_> = VIDEO_ENCODERS
        .iter()
        .map(|e| {
            let id = e.id;
            std::thread::spawn(move || gst_has(id) && encoder_works(id))
        })
        .collect();
    VIDEO_ENCODERS
        .iter()
        .zip(handles)
        .filter_map(|(e, h)| h.join().unwrap_or(false).then(|| e.clone()))
        .collect()
}

fn encoder_works(id: &str) -> bool {
    let mut args: Vec<String> = ["-q", "videotestsrc", "num-buffers=3", "!", "video/x-raw,width=640,height=360", "!", "videoconvert", "!"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(video_encoder_fragment(id, 2000));
    args.extend(["!", "fakesink"].iter().map(|s| s.to_string()));
    let Ok(mut child) = Command::new("gst-launch-1.0").args(&args).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub fn available_audio_encoders() -> Vec<EncoderInfo> {
    AUDIO_ENCODERS.iter().filter(|e| gst_has(e.id)).cloned().collect()
}

fn video_encoder_fragment(id: &str, kbps: u32) -> Vec<String> {
    let mut v: Vec<String> = match id {
        "x264enc" => vec![id.into(), format!("bitrate={kbps}"), "speed-preset=veryfast".into(), "tune=zerolatency".into()],
        "openh264enc" => vec![id.into(), format!("bitrate={}", kbps as u64 * 1000)],
        _ => vec![id.into(), format!("bitrate={kbps}")],
    };
    v.push("!".into());
    v.push(if id.contains("265") { "h265parse" } else { "h264parse" }.into());
    v
}

fn audio_encoder_fragment(id: &str, kbps: u32) -> Vec<String> {
    match id {
        "lamemp3enc" => vec![id.into(), "target=bitrate".into(), format!("bitrate={kbps}"), "cbr=true".into(), "!".into(), "mpegaudioparse".into()],
        "opusenc" => vec![id.into(), format!("bitrate={}", kbps * 1000)],
        _ => vec![id.into(), format!("bitrate={}", kbps * 1000), "!".into(), "aacparse".into()],
    }
}

#[derive(Debug, Clone)]
pub struct RecordConfig {
    pub path: PathBuf,
    pub container: Container,
    pub video_encoder: String,
    pub video_kbps: u32,
    pub audio_encoder: Option<String>,
    pub audio_kbps: u32,
    pub alsa_card: Option<u32>,
    /// Monitor audio through the recording pipeline (the ALSA PCM can only
    /// be opened once, so the standalone monitor is paused while recording).
    pub monitor_volume: Option<f32>,
}

pub fn build_args(cfg: &RecordConfig, fmt: &FrameFormat, fps: (u32, u32), full_range: bool, hdr: bool) -> Result<Vec<String>> {
    let format = match fmt.fourcc {
        PIX_YUYV => "yuy2",
        PIX_BGR24 => "bgr",
        PIX_NV12 => "nv12",
        f => bail!("cannot record pixel format {}", crate::v4l2::fourcc_str(f)),
    };
    let mut a: Vec<String> = vec!["-e".into(), "-q".into()];
    let mut push = |s: &[&str]| a.extend(s.iter().map(|x| x.to_string()));
    push(&["fdsrc", "fd=0", "!"]);
    let mut raw = vec![
        "rawvideoparse".to_string(),
        format!("format={format}"),
        format!("width={}", fmt.width),
        format!("height={}", fmt.height),
        format!("framerate={}/{}", fps.0, fps.1),
    ];
    if fmt.fourcc != PIX_BGR24 {
        // range:matrix:transfer:primaries — 1=full/2=limited, BT.709 or BT.2020/PQ.
        let range = if full_range { 1 } else { 2 };
        raw.push(if hdr { format!("colorimetry={range}:6:14:7") } else { format!("colorimetry={range}:3:5:1") });
    }
    a.extend(raw);
    let mut push = |s: &[&str]| a.extend(s.iter().map(|x| x.to_string()));
    push(&["!", "queue", "max-size-buffers=4", "max-size-bytes=0", "max-size-time=0", "!", "videoconvert", "n-threads=0", "!"]);
    a.extend(video_encoder_fragment(&cfg.video_encoder, cfg.video_kbps));
    a.extend(["!", "queue", "!", "mux."].iter().map(|s| s.to_string()));

    if let (Some(card), Some(aenc)) = (cfg.alsa_card, cfg.audio_encoder.as_deref()) {
        if aenc == "opusenc" && cfg.container == Container::Mp4 {
            bail!("Opus audio needs the MKV container");
        }
        a.extend(
            ["alsasrc", &format!("device=plughw:{card}"), "!", "audio/x-raw,rate=48000,channels=2", "!", "queue", "!", "audioconvert", "!", "audioresample", "!", "tee", "name=at", "!", "queue", "!"]
                .iter()
                .map(|s| s.to_string()),
        );
        a.extend(audio_encoder_fragment(aenc, cfg.audio_kbps));
        a.extend(["!", "queue", "!", "mux."].iter().map(|s| s.to_string()));
        if let Some(vol) = cfg.monitor_volume {
            a.extend(
                ["at.", "!", "queue", "leaky=downstream", "!", "volume", &format!("volume={vol:.2}"), "!", "autoaudiosink", "sync=false"]
                    .iter()
                    .map(|s| s.to_string()),
            );
        }
    }
    let mux = match cfg.container {
        Container::Mkv => "matroskamux",
        Container::Mp4 => "mp4mux",
    };
    a.push(mux.into());
    a.push("name=mux".into());
    if cfg.container == Container::Mp4 {
        // Fragment so an interrupted recording is still playable.
        a.push("fragment-duration=1000".into());
    }
    a.extend(["!", "filesink"].iter().map(|s| s.to_string()));
    a.push(format!("location={}", cfg.path.display()));
    Ok(a)
}

pub struct Recorder {
    pub path: PathBuf,
    pub started: Instant,
    pub format: FrameFormat,
    tx: Option<SyncSender<Arc<Vec<u8>>>>,
    writer: Option<JoinHandle<()>>,
    child: Child,
    stderr: Arc<Mutex<String>>,
    pub dropped: Arc<AtomicU64>,
    pub written: Arc<AtomicU64>,
}

impl Recorder {
    pub fn start(cfg: &RecordConfig, fmt: FrameFormat, fps: (u32, u32), full_range: bool, hdr: bool) -> Result<Recorder> {
        if let Some(d) = cfg.path.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        let args = build_args(cfg, &fmt, fps, full_range, hdr)?;
        log::info!("gst-launch-1.0 {}", args.join(" "));
        let mut child = Command::new("gst-launch-1.0")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting gst-launch-1.0")?;
        let stdin: ChildStdin = child.stdin.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut es) = child.stderr.take() {
            let buf = stderr.clone();
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = es.read_to_string(&mut s);
                *buf.lock().unwrap() = s;
            });
        }
        // ~256 MB of headroom absorbs encoder start-up (NVENC init takes a
        // few hundred ms) without dropping: 60 frames at 1080p, 16 at 4K.
        let depth = (256 << 20) / fmt.tight_size().max(1);
        let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(depth.clamp(4, 60));
        let written = Arc::new(AtomicU64::new(0));
        let w2 = written.clone();
        let writer = std::thread::Builder::new()
            .name("rec-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                for frame in rx {
                    if stdin.write_all(&frame).is_err() {
                        break;
                    }
                    w2.fetch_add(1, Ordering::Relaxed);
                }
                // Dropping stdin sends EOF → fdsrc EOS.
            })?;
        Ok(Recorder {
            path: cfg.path.clone(),
            started: Instant::now(),
            format: fmt,
            tx: Some(tx),
            writer: Some(writer),
            child,
            stderr,
            dropped: Arc::new(AtomicU64::new(0)),
            written,
        })
    }

    /// Queues a frame `repeat` times (repeats fill sequence gaps so the
    /// constant-rate timeline stays aligned with the live audio clock).
    pub fn push(&self, frame: Arc<Vec<u8>>, repeat: u32) {
        let Some(tx) = &self.tx else { return };
        for _ in 0..repeat.max(1) {
            match tx.try_send(frame.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }

    /// Returns an error description if the pipeline died.
    pub fn check(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(st)) => {
                std::thread::sleep(Duration::from_millis(50));
                let err = self.stderr.lock().unwrap().clone();
                let last = err.lines().filter(|l| !l.trim().is_empty()).next_back().unwrap_or("").to_string();
                Some(format!("recording pipeline exited ({st}): {last}"))
            }
            _ => None,
        }
    }

    /// Flushes video, sends EOS to audio (SIGINT with `-e`) and waits for the
    /// muxer to finalize the file.
    pub fn stop(mut self) -> Result<PathBuf> {
        self.tx.take();
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        std::thread::sleep(Duration::from_millis(300));
        // SAFETY: plain kill(2) on our own child pid.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    bail!("muxer did not finish in time; {} may be incomplete", self.path.display());
                }
            }
        }
        Ok(self.path.clone())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            self.tx.take();
            // SAFETY: as above.
            unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_pipeline() {
        let cfg = RecordConfig {
            path: "/tmp/a b.mkv".into(),
            container: Container::Mkv,
            video_encoder: "nvh264enc".into(),
            video_kbps: 20000,
            audio_encoder: Some("avenc_aac".into()),
            audio_kbps: 192,
            alsa_card: Some(3),
            monitor_volume: Some(0.5),
        };
        let fmt = FrameFormat { fourcc: PIX_YUYV, width: 1920, height: 1080, stride: 3840 };
        let a = build_args(&cfg, &fmt, (60, 1), false, false).unwrap();
        let s = a.join(" ");
        assert!(s.contains("rawvideoparse format=yuy2 width=1920 height=1080 framerate=60/1 colorimetry=2:3:5:1"));
        assert!(s.contains("nvh264enc bitrate=20000 ! h264parse"));
        assert!(s.contains("alsasrc device=plughw:3"));
        assert!(s.contains("autoaudiosink"));
        assert_eq!(a.last().unwrap(), "location=/tmp/a b.mkv");
    }

    #[test]
    fn opus_mp4_rejected() {
        let cfg = RecordConfig {
            path: "/tmp/x.mp4".into(),
            container: Container::Mp4,
            video_encoder: "x264enc".into(),
            video_kbps: 1,
            audio_encoder: Some("opusenc".into()),
            audio_kbps: 1,
            alsa_card: Some(0),
            monitor_volume: None,
        };
        let fmt = FrameFormat { fourcc: PIX_NV12, width: 2, height: 2, stride: 2 };
        assert!(build_args(&cfg, &fmt, (30, 1), false, false).is_err());
    }
}
