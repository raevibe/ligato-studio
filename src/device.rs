//! Device discovery and the per-board capability table.
//!
//! The capability table mirrors the `bus: 2` (PCIe) entries of Elgato
//! Studio's `ElgatoDeviceCapabilities.json` for the three boards the sc0710
//! driver supports, narrowed to what the Linux driver can actually do.

use crate::v4l2;
use std::fs;
use std::path::{Path, PathBuf};

pub const SC0710_VENDOR: u16 = 0x12ab;
pub const SC0710_DEVICE: u16 = 0x0710;
pub const ELGATO_SUBVENDOR: u16 = 0x1cfa;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    Pro4K60Mk2,
    Pro4K,
    CamLinkPro,
    Unknown(u16),
}

#[derive(Debug, Clone)]
pub struct Capabilities {
    pub name: &'static str,
    pub supports_custom_edid: bool,
    pub supports_edid_mode: bool,
    pub supports_color_range: bool,
    pub supports_picture_settings: bool,
    pub supports_hdr_input: bool,
    pub supports_hdr_recording: bool,
    /// Hardware (MCU) tonemap; the Linux driver only implements it on the MK.2.
    pub supports_hw_tonemap: bool,
    pub supports_vrr: bool,
    pub multiple_inputs: bool,
    pub max_modes: &'static [(u32, u32, f32)],
}

impl Board {
    pub fn from_subsystem(subvendor: u16, subdevice: u16) -> Board {
        match (subvendor, subdevice) {
            (ELGATO_SUBVENDOR, 0x000e) => Board::Pro4K60Mk2,
            (ELGATO_SUBVENDOR, 0x0012) => Board::Pro4K,
            (ELGATO_SUBVENDOR, 0x0011) => Board::CamLinkPro,
            (_, d) => Board::Unknown(d),
        }
    }

    pub fn capabilities(self) -> Capabilities {
        match self {
            Board::Pro4K60Mk2 => Capabilities {
                name: "Game Capture 4K60 Pro MK.2",
                supports_custom_edid: true,
                supports_edid_mode: true,
                supports_color_range: true,
                supports_picture_settings: true,
                supports_hdr_input: true,
                supports_hdr_recording: true,
                supports_hw_tonemap: true,
                supports_vrr: true,
                multiple_inputs: false,
                max_modes: &[(1920, 1080, 240.0), (2560, 1440, 144.0), (4096, 2160, 60.0)],
            },
            Board::Pro4K => Capabilities {
                name: "4K Pro",
                supports_custom_edid: true,
                supports_edid_mode: true,
                supports_color_range: true,
                supports_picture_settings: true,
                supports_hdr_input: true,
                supports_hdr_recording: true,
                supports_hw_tonemap: false,
                supports_vrr: true,
                multiple_inputs: false,
                max_modes: &[(1920, 1080, 360.0), (4096, 2160, 240.0), (7680, 4320, 60.0)],
            },
            Board::CamLinkPro => Capabilities {
                name: "Cam Link Pro",
                supports_custom_edid: false,
                supports_edid_mode: false,
                supports_color_range: false,
                supports_picture_settings: true,
                supports_hdr_input: false,
                supports_hdr_recording: false,
                supports_hw_tonemap: false,
                supports_vrr: false,
                multiple_inputs: true,
                max_modes: &[(3840, 2160, 60.0)],
            },
            Board::Unknown(_) => Capabilities {
                name: "Unknown SC0710 board",
                supports_custom_edid: false,
                supports_edid_mode: false,
                supports_color_range: true,
                supports_picture_settings: true,
                supports_hdr_input: false,
                supports_hdr_recording: false,
                supports_hw_tonemap: false,
                supports_vrr: false,
                multiple_inputs: false,
                max_modes: &[],
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub video_path: PathBuf,
    pub card_name: String,
    pub driver: String,
    pub pci_path: Option<PathBuf>,
    pub board: Board,
    /// ALSA card index whose PCI parent matches this video node.
    pub alsa_card: Option<u32>,
    /// Index of this node among nodes on the same PCI device (Cam Link Pro inputs).
    pub input_index: usize,
}

impl DeviceInfo {
    pub fn label(&self) -> String {
        if !self.is_sc0710() {
            return format!("{} [{}] ({})", self.card_name, self.driver, self.video_path.display());
        }
        let caps = self.board.capabilities();
        if caps.multiple_inputs {
            format!("{} — HDMI {} ({})", caps.name, self.input_index + 1, self.video_path.display())
        } else {
            format!("{} ({})", caps.name, self.video_path.display())
        }
    }

    pub fn is_sc0710(&self) -> bool {
        self.driver == "sc0710"
    }
}

fn read_hex(p: &Path) -> Option<u16> {
    let s = fs::read_to_string(p).ok()?;
    u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

fn pci_parent(sysdev: &Path) -> Option<PathBuf> {
    let mut p = fs::canonicalize(sysdev).ok()?;
    loop {
        if p.join("subsystem_device").exists() && p.join("vendor").exists() {
            return Some(p);
        }
        if !p.pop() {
            return None;
        }
    }
}

fn alsa_card_for_pci(pci: &Path) -> Option<u32> {
    let rd = fs::read_dir("/sys/class/sound").ok()?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(idx) = name.strip_prefix("card").and_then(|n| n.parse::<u32>().ok()) else { continue };
        if let Some(parent) = pci_parent(&e.path().join("device")) {
            if parent == pci {
                return Some(idx);
            }
        }
    }
    None
}

/// Lists every V4L2 capture node driven by sc0710. When `include_others` is
/// set, other capture devices (e.g. UVC) are listed too as generic sources.
pub fn discover(include_others: bool) -> Vec<DeviceInfo> {
    let mut nodes: Vec<(u32, PathBuf)> = fs::read_dir("/dev")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.strip_prefix("video").and_then(|i| i.parse::<u32>().ok()).map(|i| (i, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();

    let mut out: Vec<DeviceInfo> = Vec::new();
    for (idx, path) in nodes {
        let Ok(dev) = v4l2::Device::open(&path) else { continue };
        let Ok(cap) = dev.querycap() else { continue };
        if cap.device_caps & v4l2::CAP_VIDEO_CAPTURE == 0 {
            continue;
        }
        let is_sc = cap.driver == "sc0710";
        if !is_sc && !include_others {
            continue;
        }
        let pci = pci_parent(Path::new(&format!("/sys/class/video4linux/video{idx}/device")));
        let board = match &pci {
            Some(p) if is_sc => {
                let sv = read_hex(&p.join("subsystem_vendor")).unwrap_or(0);
                let sd = read_hex(&p.join("subsystem_device")).unwrap_or(0);
                Board::from_subsystem(sv, sd)
            }
            _ => Board::Unknown(0),
        };
        let alsa_card = pci.as_deref().and_then(alsa_card_for_pci);
        let input_index = out.iter().filter(|d| d.pci_path.is_some() && d.pci_path == pci).count();
        out.push(DeviceInfo {
            video_path: path,
            card_name: cap.card,
            driver: cap.driver,
            pci_path: pci,
            board,
            alsa_card,
            input_index,
        });
    }
    out
}

/// True when the sc0710 module is loaded at all (even with no card bound).
pub fn driver_loaded() -> bool {
    Path::new("/sys/module/sc0710").exists()
}

/// PCI functions that look like SC0710 boards, bound or not. Used to explain
/// "card present but no video node" situations (e.g. missing ECP5 firmware).
pub fn sc0710_pci_functions() -> Vec<(PathBuf, u16, bool)> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir("/sys/bus/pci/devices") {
        for e in rd.flatten() {
            let p = e.path();
            if read_hex(&p.join("vendor")) == Some(SC0710_VENDOR) && read_hex(&p.join("device")) == Some(SC0710_DEVICE) {
                let sd = read_hex(&p.join("subsystem_device")).unwrap_or(0);
                out.push((p.clone(), sd, p.join("driver").exists()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn board_ids() {
        assert_eq!(Board::from_subsystem(0x1cfa, 0x000e), Board::Pro4K60Mk2);
        assert_eq!(Board::from_subsystem(0x1cfa, 0x0012), Board::Pro4K);
        assert_eq!(Board::from_subsystem(0x1cfa, 0x0011), Board::CamLinkPro);
        assert!(matches!(Board::from_subsystem(0x1cfa, 0x0006), Board::Unknown(6)));
    }
}
