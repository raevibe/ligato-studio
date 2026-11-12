//! PCIe link bandwidth vs. what the current signal needs.
//!
//! The 4K60 Pro MK.2 is a PCIe 2.0 x4 card. In a slot wired for fewer lanes
//! (common for chipset slots) it still enumerates, but at 4K60 most of every
//! frame never arrives and capture apps show black frames with thin strips of
//! picture. This module spots that before the user goes hunting for a driver
//! bug.

use std::fs;
use std::path::Path;

/// Rough share of raw PCIe payload bandwidth a capture DMA actually gets
/// (TLP headers, flow control, descriptor fetches).
const PROTOCOL_EFFICIENCY: f64 = 0.80;
/// Above this share of usable bandwidth, capture is marginal.
const TIGHT: f64 = 0.85;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PciLink {
    pub gts: f64,
    pub width: u32,
    /// What the card itself supports.
    pub card_max_gts: f64,
    pub card_max_width: u32,
    /// Widest link the slot (upstream port) can provide, if known.
    pub slot_max_width: Option<u32>,
}

fn read_gts(p: &Path) -> Option<f64> {
    // e.g. "5.0 GT/s PCIe" or "8.0 GT/s"
    fs::read_to_string(p).ok()?.split_whitespace().next()?.parse().ok()
}

fn read_u32(p: &Path) -> Option<u32> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

impl PciLink {
    /// Reads the negotiated link of a PCI function from sysfs.
    pub fn read(pci: &Path) -> Option<PciLink> {
        let gts = read_gts(&pci.join("current_link_speed"))?;
        let width = read_u32(&pci.join("current_link_width"))?;
        if gts <= 0.0 || width == 0 {
            return None;
        }
        let slot_max_width = fs::canonicalize(pci)
            .ok()
            .and_then(|p| p.parent().map(|u| u.join("max_link_width")))
            .and_then(|p| read_u32(&p));
        Some(PciLink {
            gts,
            width,
            card_max_gts: read_gts(&pci.join("max_link_speed")).unwrap_or(gts),
            card_max_width: read_u32(&pci.join("max_link_width")).unwrap_or(width),
            slot_max_width,
        })
    }

    pub fn generation(gts: f64) -> u32 {
        match gts {
            g if g < 3.0 => 1,
            g if g < 6.0 => 2,
            g if g < 10.0 => 3,
            g if g < 20.0 => 4,
            g if g < 40.0 => 5,
            _ => 6,
        }
    }

    fn payload_bytes_per_sec(gts: f64, width: u32) -> f64 {
        // Gen1/2 use 8b/10b line coding, Gen3+ 128b/130b.
        let coding = if gts < 6.0 { 0.8 } else { 128.0 / 130.0 };
        gts * 1e9 * coding / 8.0 * width as f64
    }

    /// Estimated bytes/s a capture DMA can sustain on this link.
    pub fn usable_bytes_per_sec(&self) -> f64 {
        Self::payload_bytes_per_sec(self.gts, self.width) * PROTOCOL_EFFICIENCY
    }

    /// True when the card negotiated fewer lanes or a lower speed than it supports.
    pub fn degraded(&self) -> bool {
        self.width < self.card_max_width || self.gts + 0.1 < self.card_max_gts
    }

    /// True when the slot itself can't give the card all its lanes.
    pub fn slot_limited(&self) -> bool {
        self.slot_max_width.map(|w| w < self.card_max_width).unwrap_or(false)
    }

    pub fn describe(&self) -> String {
        let mut s = format!("Gen {} x{}", Self::generation(self.gts), self.width);
        if self.degraded() {
            s += &format!(
                " (card supports Gen {} x{})",
                Self::generation(self.card_max_gts),
                self.card_max_width
            );
        }
        s += &format!(", ~{:.0} MB/s usable", self.usable_bytes_per_sec() / 1e6);
        s
    }
}

/// Bytes per pixel of the formats the capture drivers deliver.
pub fn bytes_per_pixel(fourcc: u32) -> Option<f64> {
    match &fourcc.to_le_bytes() {
        b"YUYV" | b"UYVY" => Some(2.0),
        b"BGR3" | b"RGB3" => Some(3.0),
        b"NV12" => Some(1.5),
        b"AR24" | b"XR24" | b"BA24" | b"BX24" => Some(4.0),
        _ => None,
    }
}

pub fn needed_bytes_per_sec(width: u32, height: u32, fps: f64, fourcc: u32) -> Option<f64> {
    let bpp = bytes_per_pixel(fourcc)?;
    if fps <= 0.0 || width == 0 || height == 0 {
        return None;
    }
    Some(width as f64 * height as f64 * bpp * fps)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Ok,
    /// Fits, but with little margin.
    Tight,
    /// Doesn't fit: expect black or torn frames.
    Over,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub needed: f64,
    pub usable: f64,
    pub verdict: Verdict,
    /// What to do about it, for Tight/Over.
    pub advice: Option<String>,
}

const YUYV: u32 = u32::from_le_bytes(*b"YUYV");

/// Compares a signal against the link. `fourcc` is the format the driver is
/// delivering; `can_yuyv` says whether switching to YUYV is an option.
pub fn check(link: &PciLink, width: u32, height: u32, fps: f64, fourcc: u32, can_yuyv: bool) -> Option<Check> {
    let needed = needed_bytes_per_sec(width, height, fps, fourcc)?;
    let usable = link.usable_bytes_per_sec();
    let ratio = needed / usable;
    let verdict = if ratio > 1.0 {
        Verdict::Over
    } else if ratio > TIGHT {
        Verdict::Tight
    } else {
        Verdict::Ok
    };
    let advice = (verdict != Verdict::Ok).then(|| advise(link, width, height, fps, fourcc, can_yuyv, usable));
    Some(Check { needed, usable, verdict, advice })
}

fn fits(w: u32, h: u32, fps: f64, fourcc: u32, usable: f64) -> bool {
    needed_bytes_per_sec(w, h, fps, fourcc).map(|n| n <= usable * TIGHT).unwrap_or(false)
}

fn advise(link: &PciLink, width: u32, height: u32, fps: f64, fourcc: u32, can_yuyv: bool, usable: f64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if fourcc != YUYV && can_yuyv && fits(width, height, fps, YUYV, usable) {
        parts.push("Switch the pixel format to YUYV; that fits.".into());
    } else {
        let fmt = if can_yuyv { YUYV } else { fourcc };
        let modes: [(u32, u32, f64, &str); 6] = [
            (3840, 2160, 30.0, "4K30"),
            (2560, 1440, 60.0, "1440p60"),
            (1920, 1080, 120.0, "1080p120"),
            (1920, 1080, 60.0, "1080p60"),
            (1280, 720, 60.0, "720p60"),
            (1920, 1080, 30.0, "1080p30"),
        ];
        let current_px = width as f64 * height as f64 * fps;
        let ok: Vec<&str> = modes
            .iter()
            .filter(|(w, h, f, _)| (*w as f64 * *h as f64 * f) < current_px && fits(*w, *h, *f, fmt, usable))
            .map(|m| m.3)
            .take(3)
            .collect();
        if !ok.is_empty() {
            parts.push(format!(
                "Set the source to {}{}.",
                ok.join(", "),
                if can_yuyv { " and use YUYV" } else { "" }
            ));
        }
    }
    if link.slot_limited() {
        parts.push(format!(
            "This slot only has {} lanes; a slot with x{} or more would fix it.",
            link.slot_max_width.unwrap_or(link.width),
            link.card_max_width
        ));
    } else if link.degraded() {
        parts.push(format!(
            "The card linked at x{} instead of x{}: reseat it, or check the slot's lane setting in the BIOS.",
            link.width, link.card_max_width
        ));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BGR3: u32 = u32::from_le_bytes(*b"BGR3");

    // The user's machine: 4K60 Pro MK.2 (Gen2 x4) in a B650 chipset slot wired x2.
    fn x2_slot() -> PciLink {
        PciLink { gts: 5.0, width: 2, card_max_gts: 5.0, card_max_width: 4, slot_max_width: Some(2) }
    }

    #[test]
    fn usable_bandwidth() {
        assert!((x2_slot().usable_bytes_per_sec() / 1e6 - 800.0).abs() < 1.0);
        let g3 = PciLink { gts: 8.0, width: 1, card_max_gts: 8.0, card_max_width: 1, slot_max_width: None };
        assert!((g3.usable_bytes_per_sec() / 1e6 - 787.7).abs() < 1.0);
        assert!(x2_slot().degraded() && x2_slot().slot_limited());
        assert_eq!(x2_slot().describe(), "Gen 2 x2 (card supports Gen 2 x4), ~800 MB/s usable");
    }

    #[test]
    fn uhd60_bgr_over_on_x2() {
        let c = check(&x2_slot(), 3840, 2160, 60.0, BGR3, true).unwrap();
        assert_eq!(c.verdict, Verdict::Over);
        let a = c.advice.unwrap();
        assert!(a.contains("4K30") && a.contains("YUYV") && a.contains("only has 2 lanes"), "{a}");
        assert!(!a.contains("Switch the pixel format"), "YUYV 4K60 doesn't fit either: {a}");
    }

    #[test]
    fn uhd30_bgr_suggests_yuyv() {
        let c = check(&x2_slot(), 3840, 2160, 30.0, BGR3, true).unwrap();
        assert_eq!(c.verdict, Verdict::Tight);
        assert!(c.advice.unwrap().starts_with("Switch the pixel format to YUYV"));
    }

    #[test]
    fn fits_on_full_link() {
        let x4 = PciLink { width: 4, slot_max_width: Some(4), ..x2_slot() };
        assert_eq!(check(&x4, 3840, 2160, 60.0, YUYV, true).unwrap().verdict, Verdict::Ok);
        assert_eq!(check(&x2_slot(), 1920, 1080, 60.0, BGR3, true).unwrap().verdict, Verdict::Ok);
    }
}
