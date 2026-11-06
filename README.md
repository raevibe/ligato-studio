# Ligato Studio

A Linux capture app written in Rust for Elgato's SC0710-based PCIe cards (4K60 Pro MK.2, 4K Pro,
Cam Link Pro). It runs on top of the [sc0710 driver](https://github.com/Nakildias/sc0710) and
covers the capture side of Elgato Studio 2.0 for these cards. How each Studio feature maps to
the driver is documented in [docs/REVERSE_ENGINEERING.md](docs/REVERSE_ENGINEERING.md).

Not affiliated with or endorsed by Elgato/Corsair.

## Features

| Elgato Studio feature | Here | How |
|---|---|---|
| Live preview | ✓ | V4L2 mmap streaming, GPU-drawn with egui |
| Signal info | ✓ | `VIDIOC_QUERY_DV_TIMINGS` + `/proc/sc0710-state` |
| EDID mode (Internal / Display / Merged) | ✓ | driver `EDID Source` control |
| Custom EDID | ✓ | `VIDIOC_S_EDID` from a `.bin`, with automatic backup and restore |
| HDR tonemapping (Auto / Hardware / Software / Both) | ✓ | Studio's policy, ported from IL; drives `hw_tonemap`/`sw_tonemap`/`hdr_bgr24` |
| Video color range (Bypass / Expand / Shrink) | ✓ | software (the driver has no hardware path) |
| Picture settings | ✓ | software (the driver reads procamp but can't write it) |
| RGB 4:4:4 capture | ✓ | BGR24 pixel format |
| Audio monitoring | ✓ | card ALSA PCM → default output |
| Recording | ✓ | MKV/MP4, NVENC / VA-API / x264 / OpenH264 + AAC/Opus/MP3 via `gst-launch-1.0` |
| Snapshots | ✓ | full-resolution PNG |
| Firmware update, Prompter, Voice Sync, YouTube | ✗ | out of scope / no driver support |

## Install

On any x86_64 Linux machine, this downloads the latest release and adds **Ligato Studio** to your
applications menu. No root needed; everything goes in `~/.local`.

```bash
curl -fsSL https://raw.githubusercontent.com/raevibe/ligato-studio/main/install.sh | sh
```

Add `-s -- --all-devices` after `sh` to also list non-sc0710 capture devices (such as a USB
Cam Link) in the launcher. Run the same command again to update, or with `-s -- --uninstall`
to remove it. The installer tells you if GStreamer, the sc0710 driver or `video` group
membership is missing.

## Requirements

- The sc0710 kernel module, loaded (`sudo modprobe sc0710`)
- GStreamer 1.x runtime with `gst-launch-1.0` and plugins-base/good, plus an H.264/HEVC
  encoder (NVENC `nvcodec`, `va`, `x264` or `openh264`). Encoders are test-run at startup and
  only working ones are offered.
- Your user in the `video` group (for `/dev/video*`)
- Optional: `pkexec` (polkit) or the udev rule in `contrib/`, for changing HDR driver parameters

### Building from source

Building needs only a Rust toolchain (https://rustup.rs): no GTK, GStreamer or ALSA
development headers.

```bash
git clone https://github.com/raevibe/ligato-studio.git
```

```bash
cd ligato-studio && ./install.sh
```

`./install.sh` builds with `cargo build --release` if needed and adds the app to your menu.
Or run it in place with `cargo run --release`.

## Command line

```
sc0710-studio                    # GUI
sc0710-studio --list             # print detected capture devices, formats, signal, controls
sc0710-studio --record 60        # record 60 s headless using saved settings
sc0710-studio --all-devices      # also show non-sc0710 capture devices (e.g. UVC Cam Link)
```

Settings are stored in `~/.config/sc0710-studio/settings.json`. Recordings and snapshots go to
`~/Videos/sc0710-studio/` by default.

## Driver parameters without a password prompt

HDR tonemapping is set through `/sys/module/sc0710/parameters/*`, which only root can write.
The app asks through `pkexec`, batching all changes into one prompt. To skip the prompt,
install the udev rule:

```bash
sudo cp contrib/99-sc0710-studio.rules /etc/udev/rules.d/
```

## Notes and limitations

- The pixel format is device-wide in the driver: switching YUYV ↔ RGB fails with `EBUSY` while
  another app is capturing.
- The driver only produces audio while video streams (unless `keep_audio_alive=1`).
- The ALSA PCM opens exclusively. While recording, monitoring runs through the recording
  pipeline.
- HDR passthrough recordings use 8-bit BGR24 data tagged BT.2020/PQ (the driver has no P010).
- Recordings use a constant frame rate. Frames are placed on the timeline by their capture
  timestamp (duplicated or dropped) to stay in sync with the audio clock.
- The Cam Link Pro shows one device per HDMI input. Its EDID controls aren't mapped by the
  driver yet.

## Layout

```
src/v4l2.rs      raw V4L2 ioctls (ABI sizes unit-tested)
src/device.rs    discovery, board identification, capability table
src/driver.rs    sysfs parameters (allow-listed) and /proc/sc0710-state parser
src/capture.rs   capture thread: stream, source-change handling, preview, recorder feed
src/convert.rs   color range + picture processing, RGBA preview conversion
src/recorder.rs  gst-launch pipeline builder, encoder probing, A/V recording
src/tonemap.rs   Elgato Studio tonemapping policy (ported from IL)
src/edid.rs      EDID validation and parsing
src/app.rs       egui UI
install.sh       per-user installer (release download or source build)
.github/         release workflow
tools/           .NET metadata/IL dump scripts used for the analysis
```

## Releasing

Pushing a tag such as `v0.1.0` runs `.github/workflows/release.yml`, which tests, builds on
Ubuntu 22.04 (so the binary runs on older glibc too), and publishes
`ligato-studio-x86_64-linux.tar.gz` as a GitHub release. `install.sh` always fetches the latest.

```bash
git tag v0.1.1 && git push origin v0.1.1
```

## License

MIT. See [LICENSE](LICENSE).
