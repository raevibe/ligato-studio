//! HDR tonemapping policy, ported from Elgato Studio's
//! `Elgato.Studio.Models.VideoDevice.TonemappingPolicy` (decoded from IL),
//! and its mapping onto the sc0710 driver's module parameters.
//!
//! IL of `DetermineTonemappingState(isInputHdr, isHdrMode, keepHdr, mode)`:
//!   HardwareOnly → (hw=1, sw=0); SoftwareOnly → (0, 1); Both → (1, 1)
//!   Auto → if !(isInputHdr && isHdrMode) → (1, 0) else (0, !keepHdr)
//! `IsEffectiveHdrMode(a, b)` is `a & b`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TonemappingMode {
    #[default]
    Auto,
    HardwareOnly,
    SoftwareOnly,
    Both,
    /// Not in Elgato Studio's enum: no tonemap at all, i.e. HDR passthrough
    /// (BGR24 with BT.2020/PQ tags). Exposed because on Linux recording HDR
    /// requires it explicitly.
    Off,
}

impl TonemappingMode {
    pub const ALL: [TonemappingMode; 5] = [
        TonemappingMode::Auto,
        TonemappingMode::HardwareOnly,
        TonemappingMode::SoftwareOnly,
        TonemappingMode::Both,
        TonemappingMode::Off,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TonemappingMode::Auto => "Auto",
            TonemappingMode::HardwareOnly => "Hardware (card MCU)",
            TonemappingMode::SoftwareOnly => "Software (driver CPU)",
            TonemappingMode::Both => "Both",
            TonemappingMode::Off => "Off (HDR passthrough)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TonemappingState {
    pub hardware: bool,
    pub software: bool,
}

pub fn is_effective_hdr_mode(input_hdr: bool, hdr_mode: bool) -> bool {
    input_hdr & hdr_mode
}

pub fn determine_state(input_hdr: bool, hdr_mode: bool, keep_hdr: bool, mode: TonemappingMode) -> TonemappingState {
    let s = |hardware, software| TonemappingState { hardware, software };
    match mode {
        TonemappingMode::HardwareOnly => s(true, false),
        TonemappingMode::SoftwareOnly => s(false, true),
        TonemappingMode::Both => s(true, true),
        TonemappingMode::Off => s(false, false),
        TonemappingMode::Auto => {
            if !is_effective_hdr_mode(input_hdr, hdr_mode) {
                s(true, false)
            } else {
                s(false, !keep_hdr)
            }
        }
    }
}

/// Driver parameter values (`hw_tonemap`, `sw_tonemap`, `hdr_bgr24`) for a
/// state. 1 = "auto", which the driver only applies while HDMI is HDR-PQ, so
/// enabling it on SDR input is harmless (as in Studio's Auto policy).
pub fn driver_params(state: TonemappingState, hw_supported: bool) -> [(&'static str, i64); 3] {
    let hw = state.hardware && hw_supported;
    // If hardware was requested but unavailable, fall back to software so the
    // preview is still SDR, matching Studio's "something tonemaps" intent.
    let sw = state.software || (state.hardware && !hw_supported);
    [
        ("hw_tonemap", hw as i64),
        ("sw_tonemap", sw as i64),
        // Passthrough prefers native BGR24 4:4:4 with BT.2020/PQ tags.
        ("hdr_bgr24", 1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use TonemappingMode::*;

    #[test]
    fn auto_policy_matches_il() {
        assert_eq!(determine_state(false, true, false, Auto), TonemappingState { hardware: true, software: false });
        assert_eq!(determine_state(true, false, false, Auto), TonemappingState { hardware: true, software: false });
        assert_eq!(determine_state(true, true, false, Auto), TonemappingState { hardware: false, software: true });
        assert_eq!(determine_state(true, true, true, Auto), TonemappingState { hardware: false, software: false });
    }

    #[test]
    fn hw_fallback() {
        let p = driver_params(TonemappingState { hardware: true, software: false }, false);
        assert_eq!(p[0], ("hw_tonemap", 0));
        assert_eq!(p[1], ("sw_tonemap", 1));
    }
}
