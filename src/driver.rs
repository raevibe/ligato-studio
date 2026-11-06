//! sc0710 driver side channels: module parameters in sysfs and the
//! world-readable `/proc/sc0710-state` status file.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

const PARAM_DIR: &str = "/sys/module/sc0710/parameters";

/// Parameters this app is allowed to change. Anything else is refused so the
/// privileged write path can never be pointed somewhere arbitrary.
pub const WRITABLE_PARAMS: &[&str] = &[
    "hw_tonemap",
    "sw_tonemap",
    "hdr_bgr24",
    "force_quantization",
    "color_deep",
    "keep_audio_alive",
    "use_status_images",
];

pub fn read_param(name: &str) -> Option<i64> {
    let s = fs::read_to_string(format!("{PARAM_DIR}/{name}")).ok()?;
    let t = s.trim();
    t.parse::<i64>().ok().or(match t {
        "Y" => Some(1),
        "N" => Some(0),
        _ => None,
    })
}

/// Writes several module parameters. Tries a direct write first (works when
/// the udev rule from `contrib/` grants group access) and otherwise batches
/// every write into a single `pkexec` prompt.
pub fn write_params(values: &[(&str, i64)]) -> Result<()> {
    for (name, _) in values {
        if !WRITABLE_PARAMS.contains(name) {
            bail!("refusing to write unknown parameter {name}");
        }
    }
    let mut pending = Vec::new();
    for &(name, v) in values {
        if read_param(name) == Some(v) {
            continue;
        }
        let path = format!("{PARAM_DIR}/{name}");
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(mut f) => {
                f.write_all(format!("{v}\n").as_bytes()).with_context(|| format!("writing {path}"))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => pending.push((name, v)),
            Err(e) => return Err(e).with_context(|| format!("opening {path}")),
        }
    }
    if pending.is_empty() {
        return Ok(());
    }
    // Names are from the allowlist and values are integers, so the script
    // below cannot be injected into.
    let script: String = pending
        .iter()
        .map(|(n, v)| format!("echo {v} > {PARAM_DIR}/{n}; "))
        .collect();
    let status = Command::new("pkexec")
        .args(["/bin/sh", "-c", &script])
        .stdin(Stdio::null())
        .status()
        .context("running pkexec (is polkit installed?)")?;
    if !status.success() {
        bail!("pkexec write was cancelled or failed ({status})");
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct ProcState {
    #[allow(dead_code)]
    pub name: String,
    pub locked: bool,
    pub format_name: Option<String>,
    pub width: u32,
    pub height: u32,
    pub interlaced: bool,
    pub fps: Option<f32>,
    pub fourcc: Option<String>,
    pub colorimetry: Option<String>,
    pub colorspace: Option<String>,
    pub eotf: Option<String>,
    pub deliver: Option<String>,
    pub tonemap_mode: Option<String>,
    pub active_10bit: Option<bool>,
    pub procamp: BTreeMap<String, i32>,
    pub raw: String,
}

impl ProcState {
    pub fn is_hdr(&self) -> bool {
        matches!(self.eotf.as_deref(), Some("HDR-PQ") | Some("HLG"))
    }
}

/// Parses `/proc/sc0710-state`; one entry per card in driver order.
pub fn parse_proc_state(text: &str) -> Vec<ProcState> {
    let mut out: Vec<ProcState> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            out.push(ProcState { name: line.trim().to_string(), ..Default::default() });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        cur.raw.push_str(line);
        cur.raw.push('\n');
        let Some((key, val)) = line.split_once(':') else { continue };
        let key = key.trim();
        let val = val.trim();
        match key {
            "HDMI" if val != "no signal" && !cur.locked => {
                // "<fmt name> -- 3840x2160p (4400x2250)"
                cur.locked = true;
                if let Some((name, rest)) = val.split_once(" -- ") {
                    cur.format_name = Some(name.to_string());
                    let geom = rest.split_whitespace().next().unwrap_or("");
                    let (w, h) = geom.split_once('x').unwrap_or(("0", "0"));
                    cur.width = w.parse().unwrap_or(0);
                    cur.interlaced = h.ends_with('i');
                    cur.height = h.trim_end_matches(['i', 'p']).parse().unwrap_or(0);
                }
            }
            "req bytes/s" => {
                // "... (N MB/s, YUYV @ 59.94 fps)"
                if let Some(inner) = val.split_once(", ").map(|x| x.1) {
                    let mut it = inner.split_whitespace();
                    cur.fourcc = it.next().map(str::to_string);
                    let _at = it.next();
                    cur.fps = it.next().and_then(|f| f.parse().ok());
                }
            }
            "colorimetry" => cur.colorimetry = Some(val.to_string()),
            "colorspace" => cur.colorspace = Some(val.to_string()),
            "eotf" => cur.eotf = Some(val.to_string()),
            "color_deep" => {
                for kv in val.split_whitespace() {
                    match kv.split_once('=') {
                        Some(("active_10bit", v)) => cur.active_10bit = Some(v == "1"),
                        Some(("deliver", v)) => cur.deliver = Some(v.to_string()),
                        Some(("mode", v)) => cur.tonemap_mode = Some(v.to_string()),
                        _ => {}
                    }
                }
            }
            "procamp" => {
                let mut it = val.split_whitespace();
                if let (Some(k), Some(v)) = (it.next(), it.next().and_then(|v| v.parse().ok())) {
                    cur.procamp.insert(k.to_string(), v);
                }
            }
            _ => {}
        }
    }
    out
}

pub fn read_proc_state() -> Vec<ProcState> {
    fs::read_to_string("/proc/sc0710-state").map(|t| parse_proc_state(&t)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state() {
        let t = "sc0710[0]\n         irq: delivered 1, missed 0, completions 1, status 00000000\n         fmt: 00000000deadbeef\n        HDMI: 3840x2160p60 -- 3840x2160p (4400x2250)\n   framesize: 16588800\n  req bytes/s: 995328000 (995 MB/s, YUYV @ 60.00 fps)\n colorimetry: BT.2020\n  colorspace: YUV422\n        eotf: HDR-PQ\n  color_deep: prefer=2 active_10bit=1 deliver=YUYV mode=sw-tonemap\n     procamp: brightness  128\n     procamp: contrast    128\n     procamp: saturation  128\n     procamp: hue         0\n";
        let s = parse_proc_state(t);
        assert_eq!(s.len(), 1);
        let s = &s[0];
        assert!(s.locked);
        assert_eq!((s.width, s.height, s.interlaced), (3840, 2160, false));
        assert_eq!(s.fps, Some(60.0));
        assert_eq!(s.fourcc.as_deref(), Some("YUYV"));
        assert!(s.is_hdr());
        assert_eq!(s.tonemap_mode.as_deref(), Some("sw-tonemap"));
        assert_eq!(s.procamp.get("brightness"), Some(&128));
    }

    #[test]
    fn no_signal() {
        let s = parse_proc_state("sc0710[0]\n        HDMI: no signal\n        eotf: SDR\n");
        assert!(!s[0].locked);
        assert!(!s[0].is_hdr());
    }

    #[test]
    fn refuses_unknown_param() {
        assert!(write_params(&[("../../etc/passwd", 1)]).is_err());
    }
}
