//! EDID parsing/validation for the EDID settings page.

use anyhow::{bail, Result};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdidSummary {
    pub manufacturer: String,
    pub product_code: u16,
    pub name: Option<String>,
    pub year: u16,
    pub preferred: Option<(u32, u32, f32)>,
    pub extensions: u8,
    pub rgb_only: bool,
    pub hdr_static_metadata: bool,
}

pub fn block_checksum_ok(block: &[u8]) -> bool {
    block.len() == 128 && block.iter().fold(0u8, |a, &b| a.wrapping_add(b)) == 0
}

pub fn validate(edid: &[u8]) -> Result<()> {
    if edid.len() < 128 || !edid.len().is_multiple_of(128) {
        bail!("EDID length {} is not a non-zero multiple of 128", edid.len());
    }
    if edid[0..8] != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00] {
        bail!("missing EDID header");
    }
    for (i, b) in edid.chunks(128).enumerate() {
        if !block_checksum_ok(b) {
            bail!("block {i} checksum is wrong");
        }
    }
    let ext = edid[126] as usize;
    if edid.len() < (ext + 1) * 128 {
        bail!("EDID declares {ext} extension blocks but only {} are present", edid.len() / 128 - 1);
    }
    Ok(())
}

pub fn parse(edid: &[u8]) -> Result<EdidSummary> {
    validate(edid)?;
    let m = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |v: u16| (b'A' - 1 + (v & 0x1f) as u8) as char;
    let manufacturer: String = [letter(m >> 10), letter(m >> 5), letter(m)].iter().collect();
    let mut s = EdidSummary {
        manufacturer,
        product_code: u16::from_le_bytes([edid[10], edid[11]]),
        year: 1990 + edid[17] as u16,
        extensions: edid[126],
        // Feature support byte bits 4:3 = 00 → RGB 4:4:4 only (digital input)
        rgb_only: edid[24] & 0x18 == 0,
        ..Default::default()
    };
    for d in 0..4 {
        let o = 54 + d * 18;
        let desc = &edid[o..o + 18];
        let clock = u16::from_le_bytes([desc[0], desc[1]]) as u32 * 10_000;
        if clock != 0 {
            if s.preferred.is_none() {
                let hact = desc[2] as u32 | ((desc[4] as u32 & 0xf0) << 4);
                let hblank = desc[3] as u32 | ((desc[4] as u32 & 0x0f) << 8);
                let vact = desc[5] as u32 | ((desc[7] as u32 & 0xf0) << 4);
                let vblank = desc[6] as u32 | ((desc[7] as u32 & 0x0f) << 8);
                let total = (hact + hblank) as f32 * (vact + vblank) as f32;
                let rate = if total > 0.0 { clock as f32 / total } else { 0.0 };
                s.preferred = Some((hact, vact, (rate * 100.0).round() / 100.0));
            }
        } else if desc[3] == 0xfc {
            let name: String = desc[5..18]
                .iter()
                .take_while(|&&c| c != 0x0a && c != 0)
                .map(|&c| c as char)
                .collect();
            s.name = Some(name.trim().to_string());
        }
    }
    // CTA-861 extension: look for the HDR static metadata data block.
    for blk in edid.chunks(128).skip(1) {
        if blk[0] != 0x02 {
            continue;
        }
        let dtd_start = (blk[2] as usize).min(127);
        let mut i = 4;
        while i < dtd_start {
            let tag = blk[i] >> 5;
            let len = (blk[i] & 0x1f) as usize;
            if tag == 7 && len >= 1 && i + 1 < 128 && blk[i + 1] == 0x06 {
                s.hdr_static_metadata = true;
            }
            i += 1 + len;
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix_checksum(b: &mut [u8]) {
        let sum = b[..127].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        b[127] = 0u8.wrapping_sub(sum);
    }

    #[test]
    fn parses_minimal_edid() {
        let mut e = vec![0u8; 128];
        e[0..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        // "ELG" = E(5) L(12) G(7)
        let m: u16 = (5 << 10) | (12 << 5) | 7;
        e[8..10].copy_from_slice(&m.to_be_bytes());
        e[17] = 33;
        // 1080p60 DTD: 148.5 MHz, 1920+280, 1080+45
        let d = [0x02, 0x3a, 0x80, 0x18, 0x71, 0x38, 0x2d, 0x40];
        e[54..62].copy_from_slice(&d);
        e[72 + 3] = 0xfc;
        e[72 + 5..72 + 12].copy_from_slice(b"Test\n  ");
        fix_checksum(&mut e);
        let s = parse(&e).unwrap();
        assert_eq!(s.manufacturer, "ELG");
        assert_eq!(s.name.as_deref(), Some("Test"));
        assert_eq!(s.preferred, Some((1920, 1080, 60.0)));
        assert_eq!(s.year, 2023);
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut e = vec![0u8; 128];
        e[0..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        e[127] = 1;
        assert!(validate(&e).is_err());
    }
}
