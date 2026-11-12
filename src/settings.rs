//! Persistent app settings (~/.config/sc0710-studio/settings.json).

use crate::tonemap::TonemappingMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Elgato's EGAVDS_COLOR_RANGE: Bypass / Expand (limited→full) / Shrink (full→limited).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ColorRange {
    #[default]
    Bypass,
    Expand,
    Shrink,
}

impl ColorRange {
    pub const ALL: [ColorRange; 3] = [ColorRange::Bypass, ColorRange::Expand, ColorRange::Shrink];
    pub fn label(self) -> &'static str {
        match self {
            ColorRange::Bypass => "Bypass (as received)",
            ColorRange::Expand => "Expand (limited to full)",
            ColorRange::Shrink => "Shrink (full to limited)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Picture {
    /// -100..100
    pub brightness: i32,
    /// 0..200 (%)
    pub contrast: i32,
    /// 0..200 (%)
    pub saturation: i32,
    /// -180..180 (degrees)
    pub hue: i32,
}

impl Default for Picture {
    fn default() -> Self {
        Picture { brightness: 0, contrast: 100, saturation: 100, hue: 0 }
    }
}

impl Picture {
    pub fn is_identity(&self) -> bool {
        *self == Picture::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PixelFormatPref {
    #[default]
    Yuyv,
    Bgr24,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Container {
    #[default]
    Mkv,
    Mp4,
}

impl Container {
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mkv => "mkv",
            Container::Mp4 => "mp4",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub last_device: Option<PathBuf>,
    pub pixel_format: PixelFormatPref,
    pub color_range: ColorRange,
    pub picture: Picture,
    pub tonemapping: TonemappingMode,
    /// Studio's "HDR mode": treat HDR input as HDR (vs. always SDR).
    pub hdr_mode: bool,
    /// Keep HDR when recording (passthrough instead of tonemapping in Auto).
    pub record_hdr: bool,
    pub audio_monitor: bool,
    pub monitor_volume: f32,
    pub record_dir: PathBuf,
    pub container: Container,
    pub video_encoder: String,
    pub video_bitrate_kbps: u32,
    pub audio_encoder: String,
    pub audio_bitrate_kbps: u32,
    pub preview_max_width: u32,
    pub show_all_devices: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let videos = dirs::video_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Videos"));
        Settings {
            last_device: None,
            pixel_format: PixelFormatPref::Yuyv,
            color_range: ColorRange::Bypass,
            picture: Picture::default(),
            tonemapping: TonemappingMode::Auto,
            hdr_mode: true,
            record_hdr: false,
            audio_monitor: false,
            monitor_volume: 1.0,
            record_dir: videos.join("sc0710-studio"),
            container: Container::Mkv,
            video_encoder: String::new(),
            video_bitrate_kbps: 40_000,
            audio_encoder: String::new(),
            audio_bitrate_kbps: 192,
            preview_max_width: 1280,
            show_all_devices: false,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("sc0710-studio")
}

impl Settings {
    fn path() -> PathBuf {
        config_dir().join("settings.json")
    }

    pub fn load() -> Settings {
        std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let p = Self::path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(j) = serde_json::to_vec_pretty(self) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, j).is_ok() {
                let _ = std::fs::rename(tmp, p);
            }
        }
    }
}
