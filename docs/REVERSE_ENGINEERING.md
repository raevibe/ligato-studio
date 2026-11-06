# Elgato Studio 2.0 → Linux: reverse-engineering notes

Purpose: interoperability. These notes record which Elgato Studio features apply to
SC0710-based PCIe cards and how each one maps onto the Linux
[sc0710 driver](https://github.com/Nakildias/sc0710) (rev `2283cef`, 2026-10-27).
No Elgato code or assets are copied into this project.

## Artifacts analysed

| File | SHA-256 |
|---|---|
| `Elgato.Studio_2.0.0.3364_x64.msix` | `2327336c8df8b2d0756f9a14cf778ebbe1f68d960c9d55618f011989bbb78bd1` |
| `Elgato.Studio.dll` (.NET, WinUI 3) | `78703f06f6b82b673a55006240ade8af51e7fab1605c93f771b75cc57b9c0399` |
| `EGAVDeviceSupport.dll` (native, device layer) | `c0dfc491eee207792eb5fb1de29933fd69e24e3a8067831f0ddb8a02cc814356` |
| `ElgatoDeviceCapabilities.json` | `d96b7c1d156a0a1ec62d10bfc4a16aef67588071f2468e216d37f9b163a562fa` |

Method: unzip the MSIX, dump ECMA-335 metadata (type/field/method names, enum
constants, user strings) with `tools/dnmeta.py`, dump specific IL method bodies with
`tools/ilbytes.py` and decode them by hand, and run `strings` on the native DLL.
**Not available:** a full .NET decompiler (installing the .NET SDK was blocked in this
environment) and a native disassembler (no Ghidra or rizin). Claims below are tagged:
*metadata* (enum constants/names read from metadata tables), *IL* (decoded IL bytes),
*strings* (native DLL log strings), *JSON* (capability file) or *inferred*.

## Architecture of the Windows app

- `Elgato.Studio.exe/.dll`: WinUI 3 front end (`Elgato.Studio.ViewModels.*`,
  `Services.IPreviewController`, `Services.IMediaCaptureService`), plus Prompter,
  Voice Sync (ONNX), YouTube, marketplace and Sentry telemetry.
- `Elgato.MFComponents.dll`: Media Foundation capture and preview graph.
- `EGAVDeviceSupport.dll`: native device abstraction wrapped through SWIG
  (`Elgato.EGAVDeviceSupport.*` proxy types). It exposes `CaptureDevice::Get/Set…`
  for every setting and talks to the Windows kernel driver (`SC0710.X64.SYS`) through
  KS property sets. The KS-property → MCU mapping lives in that kernel driver, which
  is **not** in this package; the Linux driver authors mapped it separately
  (e.g. "KS prop 722 → MCU 0x32 sub 0x11" in `scripts/mk2-set-tonemap.c`).

## Device table (*JSON*)

`ElgatoDeviceCapabilities.json` → `deviceCapabilities[]`, entries with `bus: 2` (PCIe),
`vendorID: 7418` (0x1CFA = PCI subsystem vendor):

| productID (subsystem) | Name | Key flags |
|---|---|---|
| 14 (0x000e) | Game Capture 4K60 Pro MK.2 | customEDID, EDIDMode, colorRange, picture, HDRAsInput, HDRRecording, HDRTonemapping, 10-bit, VRR; max 1080p240 / 1440p144 / 4096×2160p60 |
| 18 (0x0012) | 4K Pro ("aka 4K60 Pro MK.3") | as MK.2 + extendedEDID; max 1080p360 / 4096×2160p240 / 8K60 |
| 17 (0x0011) | Cam Link Pro | picture settings, multiple HDMI inputs; max 2160p60 |
| 7, 11 | 4K60 Pro rev 1/2 | different chip; not supported by sc0710 |

All three sc0710 boards share PCI ID `12ab:0710`. `src/device.rs` reproduces only
these three entries, narrowed to what the Linux driver implements.

## Settings and their Linux mapping

### EDID mode (*metadata*)
`Elgato.EGAVDeviceSupport.EGAVDS_EDID_MODE`: `INVALID=-1, INTERNAL=0, DISPLAY=1, MIX=2`
(also `Elgato.EventTracking.EDIDInputMode`: Internal/Display/Merged).
→ The driver's V4L2 menu control `EDID Source` (`V4L2_CID_USER_BASE + 0x9000`,
`lib/sc0710.h:182`) uses the same values: Internal/Display/Merged. The driver notes that
the MCU has no readback, so the app shows the last value it set.

### Custom EDID (*metadata*, *strings*)
`EGAVDS_EDID {name, filePath, edidType, description, sha256, downloadUrl,
isDownloadRequired, minimumFirmware…}`, `EGAVDS_EDID_TYPE: DEFAULT=0, CUSTOM=1,
USER=2, PASSTHROUGH=3`. The EDID catalogue is a managed, downloaded asset
(`EnsureManagedEDIDFilesAvailableAsync`, asset manifest from
`gc-updates.elgato.com/windows/estw-update/{0}/device-update-check.json.php`).
→ `VIDIOC_G_EDID` / `VIDIOC_S_EDID` (MK.2: MCU fn 0x59, runtime only; 4K Pro: EEPROM).
The app loads user-supplied `.bin` files, validates checksums, and backs up the
original first. Elgato's EDID files are **not** downloaded or bundled.

### Video color range (*metadata*, *strings*)
`EGAVDS_COLOR_RANGE: INVALID=-1, BYPASS=0, EXPAND=1, SHRINK=2`; native strings
`AT_Set_ColorRange` / `AT_Get_ColorRange_Setting`, i.e. a hardware conversion.
→ **No equivalent in the driver.** `force_quantization` only changes the V4L2
quantization *tag*. The app performs Expand (16–235→0–255) and Shrink (the reverse)
in software (`src/convert.rs`) and tags recordings with the resulting range.

### Picture settings (*strings*)
`CaptureDevice::{Get,Set}PictureSettings{Brightness,Contrast,Saturation,Hue}` with
Min/Max/Default getters (ranges come from the device at runtime; the values weren't
recovered statically).
→ The driver reads the card procamp (MCU sub-address 0x12: brightness, contrast,
saturation u8, hue s8) and prints it in `/proc/sc0710-state`, but has **no write path**.
The app applies brightness/contrast/saturation/hue in software and shows the card's
values read-only.

### HDR tonemapping (*metadata* + *IL*)
`Elgato.Studio.Models.VideoDevice.TonemappingMode`: `Auto=0, HardwareOnly=1,
SoftwareOnly=2, Both=3`. `TonemappingState {IsHardwareTonemappingEnabled,
IsSoftwareTonemappingEnabled}`.

`TonemappingPolicy.IsEffectiveHdrMode` IL `02 03 5f 2a` = `ldarg.0; ldarg.1; and; ret`.

`TonemappingPolicy.DetermineTonemappingState` IL (93 bytes):
```
05 2c 3e            ldarg.3; brfalse.s AUTO          ; mode == Auto?
05 17 59            ldarg.3; ldc.i4.1; sub
45 03 …             switch (3 targets)
  case 0 (HardwareOnly): 17 16 → new State(true,  false)
  case 1 (SoftwareOnly): 16 17 → new State(false, true)
  case 2 (Both):         17 17 → new State(true,  true)
  default:               16 16 → new State(false, false)
AUTO:
02 03 28 … 2d 08    if (!IsEffectiveHdrMode(arg0, arg1)) return new State(true, false)
16 04 16 fe01       return new State(false, arg2 == false)
```
Parameter meanings are *inferred* from the call shape: arg0 = input is HDR,
arg1 = HDR mode on, arg2 = keep HDR (e.g. HDR recording).
→ Ported verbatim to `src/tonemap.rs`. Hardware = driver `hw_tonemap` (MCU 0x32
sub 0x11, MK.2 only), software = driver `sw_tonemap` (CPU PQ→SDR), neither = HDR
passthrough (`hdr_bgr24`, BGR24 with BT.2020/PQ tags). The driver's "1 = auto"
already gates on HDR-PQ input, so the app writes a static parameter set that
doesn't flip, and re-prompt for root, on every HDR/SDR change.

### Signal status (*metadata*)
`EGAVDS_SIGNAL_STATUS {hasSignal, isInterlaced, isHDCPProtected, width, height,
frameRate}`, `EGAVDS_INPUT_COLOR_SPACE: RGB444=0, YUV422=1, YUV444=2, YUV420=3`.
→ `VIDIOC_QUERY_DV_TIMINGS` (size, interlace, pixel clock → rate) plus
`/proc/sc0710-state` (colorimetry, colorspace, EOTF, 10-bit, delivered format).
HDCP status isn't exposed by the driver.

### Audio input (*metadata*)
`EGAVDS_AUDIO_INPUT: HDMI=1, ANALOG=2`. The PCIe boards have HDMI audio only; the
driver exposes it as an ALSA card with driver id `sc0710`, matched to its video node
by PCI parent.

### Not ported
- Firmware updates (`firmwareMCU`/`firmwareFPGA`/`firmwareBase` in the JSON): the
  driver has no update path, and flashing blind can brick the card.
- Prompter, Voice Sync, YouTube upload, marketplace, product ratings, telemetry.
- VRR resampling (`supportsVRRResampling`): no driver interface.

## Hardware note for this machine
The PCIe card here is `12ab:0380`, subsystem `1cfa:0006`: a different YUAN chip
(likely the HD60 Pro). It isn't an SC0710 board, isn't in Elgato Studio's device table,
and isn't bound by the sc0710 driver. Live testing used a Cam Link 4K (UVC)
through `--all-devices`.
