//! egui front end.

use crate::audio::AudioMonitor;
use crate::bandwidth::{self, PciLink, Verdict};
use crate::capture::{self, CaptureHandle, Command, StartOptions};
use crate::device::{self, DeviceInfo};
use crate::driver::{self, ProcState};
use crate::edid::{self, EdidSummary};
use crate::recorder::{self, EncoderInfo, RecordConfig};
use crate::settings::{config_dir, ColorRange, Container, Picture, PixelFormatPref, Settings};
use crate::tonemap::{self, TonemappingMode};
use crate::v4l2::{self, DvTimings};
use eframe::egui::{self, Color32, RichText};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// `SC0710_CID_EDID_SOURCE` (V4L2_CID_USER_BASE + 0x9000) in lib/sc0710.h.
const SC0710_CID_EDID_SOURCE: u32 = 0x0098_0900 + 0x9000;

struct DeviceExtras {
    info_dev: Option<v4l2::Device>,
    edid_ctrl: Option<v4l2::CtrlInfo>,
    edid_source: Option<i32>,
    edid: Option<Result<EdidSummary, String>>,
    timings: Option<Result<DvTimings, String>>,
    link: Option<PciLink>,
    can_yuyv: bool,
}

pub struct App {
    settings: Settings,
    devices: Vec<DeviceInfo>,
    selected: Option<usize>,
    capture: Option<CaptureHandle>,
    texture: Option<egui::TextureHandle>,
    tex_serial: u64,
    extras: Option<DeviceExtras>,
    proc_state: Vec<ProcState>,
    last_poll: Instant,
    last_scan: Instant,
    log: Vec<(Instant, String)>,
    video_encoders: Vec<EncoderInfo>,
    audio_encoders: Vec<EncoderInfo>,
    monitor: Option<AudioMonitor>,
    edid_path: String,
    driver_params: Vec<(&'static str, Option<i64>)>,
    gst_ok: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, show_all: bool) -> App {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut settings = Settings::load();
        if show_all {
            settings.show_all_devices = true;
        }
        let gst_ok = recorder::gst_has("fdsrc");
        let video_encoders = if gst_ok { recorder::available_video_encoders() } else { vec![] };
        let audio_encoders = if gst_ok { recorder::available_audio_encoders() } else { vec![] };
        if !video_encoders.iter().any(|e| e.id == settings.video_encoder) {
            settings.video_encoder = video_encoders.first().map(|e| e.id.to_string()).unwrap_or_default();
        }
        if !audio_encoders.iter().any(|e| e.id == settings.audio_encoder) {
            settings.audio_encoder = audio_encoders.first().map(|e| e.id.to_string()).unwrap_or_default();
        }
        let mut app = App {
            settings,
            devices: vec![],
            selected: None,
            capture: None,
            texture: None,
            tex_serial: 0,
            extras: None,
            proc_state: vec![],
            last_poll: Instant::now() - Duration::from_secs(10),
            last_scan: Instant::now() - Duration::from_secs(10),
            log: vec![],
            video_encoders,
            audio_encoders,
            monitor: None,
            edid_path: String::new(),
            driver_params: vec![],
            gst_ok,
        };
        app.rescan(&cc.egui_ctx);
        app
    }

    fn note(&mut self, m: impl Into<String>) {
        let m = m.into();
        log::info!("{m}");
        self.log.push((Instant::now(), m));
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    fn current(&self) -> Option<&DeviceInfo> {
        self.selected.and_then(|i| self.devices.get(i))
    }

    fn rescan(&mut self, ctx: &egui::Context) {
        self.last_scan = Instant::now();
        let found = device::discover(self.settings.show_all_devices);
        let cur_path = self.current().map(|d| d.video_path.clone());
        let same = found.len() == self.devices.len()
            && found.iter().zip(&self.devices).all(|(a, b)| a.video_path == b.video_path);
        if same {
            return;
        }
        self.devices = found;
        let want = cur_path.or_else(|| self.settings.last_device.clone());
        let idx = want
            .and_then(|p| self.devices.iter().position(|d| d.video_path == p))
            .or(if self.devices.is_empty() { None } else { Some(0) });
        if idx != self.selected || self.capture.as_ref().map(|c| Some(&c.path) != self.current_path_for(idx).as_ref()).unwrap_or(true) {
            self.select(idx, ctx);
        }
    }

    fn current_path_for(&self, idx: Option<usize>) -> Option<PathBuf> {
        idx.and_then(|i| self.devices.get(i)).map(|d| d.video_path.clone())
    }

    fn select(&mut self, idx: Option<usize>, ctx: &egui::Context) {
        self.capture = None;
        self.monitor = None;
        self.extras = None;
        self.texture = None;
        self.selected = idx;
        let Some(dev) = self.current().cloned() else { return };
        self.settings.last_device = Some(dev.video_path.clone());
        self.settings.save();
        let c = ctx.clone();
        self.capture = Some(capture::start(
            dev.video_path.clone(),
            StartOptions {
                pixel_format: self.settings.pixel_format,
                range: self.settings.color_range,
                picture: self.settings.picture,
                preview_width: self.settings.preview_max_width,
                repaint: Box::new(move || c.request_repaint()),
            },
        ));
        let info_dev = v4l2::Device::open(&dev.video_path).ok();
        let edid_ctrl = info_dev.as_ref().and_then(|d| {
            d.controls().into_iter().find(|c| c.id == SC0710_CID_EDID_SOURCE || c.name == "EDID Source")
        });
        let edid_source = match (&info_dev, &edid_ctrl) {
            (Some(d), Some(c)) => d.get_ctrl(c.id).ok(),
            _ => None,
        };
        let can_yuyv = info_dev
            .as_ref()
            .map(|d| d.enum_formats().iter().any(|(f, _)| *f == u32::from_le_bytes(*b"YUYV")))
            .unwrap_or(false);
        // Only PCIe capture cards: a USB device's PCI parent is its host controller.
        let link = dev.pci_path.as_deref().filter(|_| dev.is_sc0710()).and_then(PciLink::read);
        if let Some(l) = &link {
            self.note(format!("PCIe link: {}", l.describe()));
        }
        self.extras = Some(DeviceExtras { info_dev, edid_ctrl, edid_source, edid: None, timings: None, link, can_yuyv });
        self.read_edid();
        self.refresh_driver_params();
        self.update_monitor();
        self.note(format!("Opened {}", dev.label()));
    }

    fn read_edid(&mut self) {
        let caps = self.current().map(|d| d.board.capabilities());
        let Some(ex) = self.extras.as_mut() else { return };
        if !caps.map(|c| c.supports_custom_edid).unwrap_or(false) {
            return;
        }
        ex.edid = ex.info_dev.as_ref().map(|d| match d.get_edid() {
            Ok(b) => edid::parse(&b).map_err(|e| format!("{e:#}")),
            Err(e) => Err(e.to_string()),
        });
    }

    fn refresh_driver_params(&mut self) {
        self.driver_params = driver::WRITABLE_PARAMS.iter().map(|&n| (n, driver::read_param(n))).collect();
    }

    fn poll_info(&mut self) {
        self.last_poll = Instant::now();
        self.proc_state = driver::read_proc_state();
        if let Some(ex) = self.extras.as_mut() {
            ex.timings = ex.info_dev.as_ref().map(|d| d.query_dv_timings().map_err(|e| match e.raw_os_error() {
                Some(libc::ENOLINK) | Some(libc::ENOLCK) | Some(libc::ENODATA) => "no signal".to_string(),
                Some(libc::ERANGE) => "out of range".to_string(),
                Some(libc::ENOTTY) => "not supported".to_string(),
                _ => e.to_string(),
            }));
        }
        let msgs = self.capture.as_ref().map(|c| std::mem::take(&mut c.shared.status.lock().unwrap().messages)).unwrap_or_default();
        for m in msgs {
            self.note(m);
        }
        if let Some(m) = self.monitor.as_mut() {
            if !m.alive() {
                self.monitor = None;
                self.note("Audio monitor stopped (is the ALSA device busy?)");
            }
        }
    }

    fn proc_for_current(&self) -> Option<&ProcState> {
        let cur = self.current()?;
        if !cur.is_sc0710() {
            return None;
        }
        // /proc lists cards in probe order; map by distinct PCI device order.
        let mut pcis: Vec<&PathBuf> = self.devices.iter().filter(|d| d.is_sc0710()).filter_map(|d| d.pci_path.as_ref()).collect();
        pcis.dedup();
        let idx = pcis.iter().position(|p| Some(*p) == cur.pci_path.as_ref())?;
        self.proc_state.get(idx)
    }

    fn input_is_hdr(&self) -> bool {
        self.proc_for_current().map(|p| p.is_hdr()).unwrap_or(false)
    }

    fn recording(&self) -> bool {
        self.capture.as_ref().map(|c| c.shared.status.lock().unwrap().recording.is_some()).unwrap_or(false)
    }

    fn update_monitor(&mut self) {
        let want = self.settings.audio_monitor && !self.recording();
        let card = self.current().and_then(|d| d.alsa_card);
        match (want, card) {
            (true, Some(card)) => {
                let restart = self.monitor.as_ref().map(|m| m.card != card || (m.volume - self.settings.monitor_volume).abs() > 0.01).unwrap_or(true);
                if restart {
                    self.monitor = None;
                    match AudioMonitor::start(card, self.settings.monitor_volume) {
                        Ok(m) => self.monitor = Some(m),
                        Err(e) => self.note(format!("Audio monitor failed: {e}")),
                    }
                }
            }
            _ => self.monitor = None,
        }
    }

    fn apply_processing(&mut self) {
        if let Some(c) = &self.capture {
            c.send(Command::SetProcessing(self.settings.color_range, self.settings.picture));
        }
        self.settings.save();
    }

    fn desired_tonemap_params(&self) -> Option<[(&'static str, i64); 3]> {
        let dev = self.current()?;
        let caps = dev.board.capabilities();
        if !dev.is_sc0710() || !caps.supports_hdr_input {
            return None;
        }
        // Driver values of 1 mean "auto: only while HDMI is HDR-PQ", so the
        // policy is evaluated as if the input were HDR; SDR input is left
        // untouched by the driver either way. This keeps the parameters
        // stable across signal changes (no re-prompt on every HDR toggle).
        let state = tonemap::determine_state(true, self.settings.hdr_mode, self.settings.record_hdr, self.settings.tonemapping);
        Some(tonemap::driver_params(state, caps.supports_hw_tonemap))
    }

    fn apply_tonemap(&mut self) {
        self.settings.save();
        let Some(params) = self.desired_tonemap_params() else { return };
        if self.capture.as_ref().map(|c| c.shared.status.lock().unwrap().streaming).unwrap_or(false) {
            // hdr_bgr24 / sw_tonemap are read when the format is negotiated;
            // the driver applies hw_tonemap immediately.
        }
        match driver::write_params(&params) {
            Ok(()) => self.note(format!("Driver tonemap set: {}", params.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join(", "))),
            Err(e) => self.note(format!("Could not set driver parameters: {e:#}")),
        }
        self.refresh_driver_params();
    }

    fn start_recording(&mut self) {
        let Some(dev) = self.current().cloned() else { return };
        if self.settings.video_encoder.is_empty() {
            self.note("No H.264/HEVC GStreamer encoder found");
            return;
        }
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let path = self.settings.record_dir.join(format!("capture_{stamp}.{}", self.settings.container.ext()));
        self.monitor = None;
        let cfg = RecordConfig {
            path,
            container: self.settings.container,
            video_encoder: self.settings.video_encoder.clone(),
            video_kbps: self.settings.video_bitrate_kbps,
            audio_encoder: (!self.settings.audio_encoder.is_empty()).then(|| self.settings.audio_encoder.clone()),
            audio_kbps: self.settings.audio_bitrate_kbps,
            alsa_card: dev.alsa_card,
            monitor_volume: self.settings.audio_monitor.then_some(self.settings.monitor_volume),
        };
        let hdr = self.input_is_hdr() && self.proc_for_current().and_then(|p| p.tonemap_mode.as_deref()) == Some("passthrough");
        if let Some(c) = &self.capture {
            c.send(Command::StartRecording(cfg, hdr));
        }
    }

    fn stop_recording(&mut self) {
        if let Some(c) = &self.capture {
            c.send(Command::StopRecording);
        }
    }

    // ---------------------------------------------------------------- UI --

    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.heading("Ligato Studio");
            ui.separator();
            let labels: Vec<String> = self.devices.iter().map(|d| d.label()).collect();
            let mut sel = self.selected;
            egui::ComboBox::from_id_salt("device")
                .width(380.0)
                .selected_text(sel.and_then(|i| labels.get(i).cloned()).unwrap_or_else(|| "No device".into()))
                .show_ui(ui, |ui| {
                    for (i, l) in labels.iter().enumerate() {
                        ui.selectable_value(&mut sel, Some(i), l);
                    }
                });
            if sel != self.selected {
                self.select(sel, ctx);
            }
            if ui.button("⟳").on_hover_text("Rescan devices").clicked() {
                self.devices.clear();
                self.rescan(ctx);
            }
            ui.separator();
            let rec = self.capture.as_ref().and_then(|c| c.shared.status.lock().unwrap().recording.clone());
            if let Some(r) = rec {
                if ui.button(RichText::new("Stop recording").color(Color32::WHITE).background_color(Color32::from_rgb(170, 30, 30))).clicked() {
                    self.stop_recording();
                }
                let secs = r.started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                ui.label(RichText::new(format!("REC {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)).color(Color32::from_rgb(230, 60, 60)))
                    .on_hover_text(r.path.display().to_string());
                if r.dropped > 0 {
                    ui.label(RichText::new(format!("{} dropped", r.dropped)).color(Color32::YELLOW));
                }
            } else {
                let enabled = self.capture.is_some() && self.gst_ok;
                if ui.add_enabled(enabled, egui::Button::new(RichText::new("Record").color(Color32::from_rgb(240, 80, 80)))).clicked() {
                    self.start_recording();
                }
            }
            if ui.add_enabled(self.capture.is_some(), egui::Button::new("📷 Snapshot")).clicked() {
                let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
                let p = self.settings.record_dir.join(format!("snapshot_{stamp}.png"));
                if let Some(c) = &self.capture {
                    c.send(Command::Snapshot(p));
                }
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(c) = &self.capture {
                let s = c.shared.status.lock().unwrap().clone();
                if let Some(f) = s.format {
                    ui.label(format!("{}×{} {}", f.width, f.height, v4l2::fourcc_str(f.fourcc)));
                    ui.separator();
                }
                ui.label(format!("{:.2} fps", s.measured_fps));
                ui.separator();
                ui.label(if s.full_range { "full range" } else { "limited range" });
                if let Some(e) = s.error {
                    ui.separator();
                    ui.colored_label(Color32::LIGHT_RED, e);
                }
            } else if !device::driver_loaded() {
                ui.colored_label(Color32::YELLOW, "sc0710 driver is not loaded (sudo modprobe sc0710)");
            }
            if let Some((t, m)) = self.log.last() {
                if t.elapsed() < Duration::from_secs(8) {
                    ui.separator();
                    ui.label(m);
                }
            }
        });
    }

    fn preview(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if let Some(c) = &self.capture {
            let p = c.shared.preview.lock().unwrap();
            if p.serial != self.tex_serial && p.width > 0 {
                let img = egui::ColorImage::from_rgba_unmultiplied([p.width, p.height], &p.rgba);
                self.tex_serial = p.serial;
                match &mut self.texture {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
                }
            }
        }
        let avail = ui.available_size();
        match &self.texture {
            Some(t) => {
                let ts = t.size_vec2();
                let scale = (avail.x / ts.x).min(avail.y / ts.y);
                let size = ts * scale;
                ui.centered_and_justified(|ui| {
                    ui.image((t.id(), size));
                });
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label(self.empty_text());
                });
            }
        }
    }

    fn empty_text(&self) -> String {
        if self.devices.is_empty() {
            let pci = device::sc0710_pci_functions();
            if !device::driver_loaded() {
                return "The sc0710 driver is not loaded.\nInstall it from github.com/Nakildias/sc0710 and run: sudo modprobe sc0710".into();
            }
            if let Some((p, sd, bound)) = pci.first() {
                if !bound {
                    return format!(
                        "SC0710 card {} (subsystem 1cfa:{sd:04x}) is present but not bound.\nFor the 4K Pro / Cam Link Pro, check `sudo dmesg | grep sc0710` (ECP5 firmware).",
                        p.display()
                    );
                }
            }
            return "No SC0710 capture device found.".into();
        }
        "Waiting for video…".into()
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        let Some(dev) = self.current().cloned() else {
            ui.label("No device selected.");
            return;
        };
        let caps = dev.board.capabilities();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::CollapsingHeader::new("Signal").default_open(true).show(ui, |ui| self.signal_section(ui));
            egui::CollapsingHeader::new("Video").default_open(true).show(ui, |ui| self.video_section(ui, &dev, &caps));
            if dev.is_sc0710() && caps.supports_hdr_input {
                egui::CollapsingHeader::new("HDR").default_open(true).show(ui, |ui| self.hdr_section(ui, &caps));
            }
            if dev.is_sc0710() && (caps.supports_edid_mode || caps.supports_custom_edid) {
                egui::CollapsingHeader::new("EDID").default_open(false).show(ui, |ui| self.edid_section(ui, &caps));
            }
            egui::CollapsingHeader::new("Audio").default_open(false).show(ui, |ui| self.audio_section(ui, &dev));
            egui::CollapsingHeader::new("Recording").default_open(false).show(ui, |ui| self.recording_section(ui));
            if dev.is_sc0710() {
                egui::CollapsingHeader::new("Driver").default_open(false).show(ui, |ui| self.driver_section(ui));
            }
            egui::CollapsingHeader::new("Log").default_open(false).show(ui, |ui| {
                for (t, m) in self.log.iter().rev().take(40) {
                    ui.label(format!("{:>4}s ago  {m}", t.elapsed().as_secs()));
                }
            });
        });
    }

    fn signal_section(&mut self, ui: &mut egui::Ui) {
        let timings = self.extras.as_ref().and_then(|e| e.timings.clone());
        let link = self.extras.as_ref().and_then(|e| e.link);
        let can_yuyv = self.extras.as_ref().map(|e| e.can_yuyv).unwrap_or(false);
        let fourcc = self.capture.as_ref().and_then(|c| c.shared.status.lock().unwrap().format.map(|f| f.fourcc));
        let bw = match (&link, &timings, fourcc) {
            (Some(l), Some(Ok(t)), Some(fc)) => bandwidth::check(l, t.width, t.height, t.fps(), fc, can_yuyv),
            _ => None,
        };
        egui::Grid::new("sig").num_columns(2).striped(true).show(ui, |ui| {
            match timings {
                Some(Ok(t)) => {
                    ui.label("Input");
                    ui.label(format!("{}×{}{} @ {:.2} Hz", t.width, t.height, if t.interlaced { "i" } else { "p" }, t.fps()));
                    ui.end_row();
                    ui.label("Pixel clock");
                    ui.label(format!("{:.2} MHz (total {}×{})", t.pixelclock as f64 / 1e6, t.htotal, t.vtotal));
                    ui.end_row();
                }
                Some(Err(e)) => {
                    ui.label("Input");
                    ui.label(e);
                    ui.end_row();
                }
                None => {}
            }
            if let Some(l) = &link {
                ui.label("PCIe link");
                ui.label(l.describe());
                ui.end_row();
            }
            if let (Some(c), Some(fc)) = (&bw, fourcc) {
                ui.label("Bandwidth");
                let text = format!(
                    "{:.0} of ~{:.0} MB/s ({})",
                    c.needed / 1e6,
                    c.usable / 1e6,
                    v4l2::fourcc_str(fc)
                );
                let color = match c.verdict {
                    Verdict::Ok => ui.visuals().text_color(),
                    Verdict::Tight => Color32::from_rgb(230, 180, 60),
                    Verdict::Over => Color32::from_rgb(235, 90, 80),
                };
                ui.label(RichText::new(text).color(color));
                ui.end_row();
            }
            if let Some(dev) = self.current().filter(|d| d.is_sc0710()) {
                let caps = dev.board.capabilities();
                if !caps.max_modes.is_empty() {
                    ui.label("Card maximum");
                    let m: Vec<String> = caps.max_modes.iter().map(|(w, h, r)| format!("{w}×{h}@{r}")).collect();
                    ui.label(m.join(", "));
                    ui.end_row();
                }
                if caps.supports_vrr {
                    ui.label("VRR");
                    ui.label("supported by card");
                    ui.end_row();
                }
            }
            if let Some(p) = self.proc_for_current().cloned() {
                for (k, v) in [
                    ("Colorimetry", p.colorimetry),
                    ("Colorspace", p.colorspace),
                    ("EOTF", p.eotf),
                    ("Delivering", p.deliver),
                    ("HDR path", p.tonemap_mode),
                ] {
                    if let Some(v) = v {
                        ui.label(k);
                        ui.label(v);
                        ui.end_row();
                    }
                }
                if let Some(b) = p.active_10bit {
                    ui.label("10-bit link");
                    ui.label(if b { "yes" } else { "no" });
                    ui.end_row();
                }
            }
        });
        if let Some(c) = bw.filter(|c| c.verdict != Verdict::Ok) {
            let (color, head) = match c.verdict {
                Verdict::Over => (
                    Color32::from_rgb(235, 90, 80),
                    "Not enough PCIe bandwidth for this signal: expect black or torn frames.",
                ),
                _ => (Color32::from_rgb(230, 180, 60), "PCIe bandwidth is nearly maxed out: frames may tear."),
            };
            ui.add_space(4.0);
            ui.label(RichText::new(head).color(color).strong());
            if let Some(a) = c.advice {
                ui.label(a);
            }
        }
    }

    fn video_section(&mut self, ui: &mut egui::Ui, dev: &DeviceInfo, caps: &device::Capabilities) {
        if dev.is_sc0710() {
            let mut pf = self.settings.pixel_format;
            ui.horizontal(|ui| {
                ui.label("Pixel format");
                ui.selectable_value(&mut pf, PixelFormatPref::Yuyv, "YUYV 4:2:2");
                ui.selectable_value(&mut pf, PixelFormatPref::Bgr24, "RGB 4:4:4");
            });
            if pf != self.settings.pixel_format {
                self.settings.pixel_format = pf;
                self.settings.save();
                if let Some(c) = &self.capture {
                    c.send(Command::SetPixelFormat(pf));
                }
            }
        }
        if caps.supports_color_range || !dev.is_sc0710() {
            let mut cr = self.settings.color_range;
            egui::ComboBox::from_label("Color range")
                .selected_text(cr.label())
                .show_ui(ui, |ui| {
                    for r in ColorRange::ALL {
                        ui.selectable_value(&mut cr, r, r.label());
                    }
                });
            if cr != self.settings.color_range {
                self.settings.color_range = cr;
                self.apply_processing();
            }
        }
        if caps.supports_picture_settings || !dev.is_sc0710() {
            ui.label(RichText::new("Picture settings").strong());
            let mut p = self.settings.picture;
            let mut changed = false;
            changed |= ui.add(egui::Slider::new(&mut p.brightness, -100..=100).text("Brightness")).changed();
            changed |= ui.add(egui::Slider::new(&mut p.contrast, 0..=200).text("Contrast").suffix("%")).changed();
            changed |= ui.add(egui::Slider::new(&mut p.saturation, 0..=200).text("Saturation").suffix("%")).changed();
            changed |= ui.add(egui::Slider::new(&mut p.hue, -180..=180).text("Hue").suffix("°")).changed();
            if ui.button("Reset").clicked() {
                p = Picture::default();
                changed = true;
            }
            if changed {
                self.settings.picture = p;
                self.apply_processing();
            }
            if let Some(ps) = self.proc_for_current() {
                if !ps.procamp.is_empty() {
                    let v: Vec<String> = ps.procamp.iter().map(|(k, v)| format!("{k} {v}")).collect();
                    ui.small(format!("Card procamp (read-only in driver): {}", v.join(", ")));
                }
            }
            ui.small("Applied in software to preview, snapshots and recordings.");
        }
    }

    fn hdr_section(&mut self, ui: &mut egui::Ui, caps: &device::Capabilities) {
        let mut s = self.settings.clone();
        let mut changed = false;
        egui::ComboBox::from_label("Tonemapping")
            .selected_text(s.tonemapping.label())
            .show_ui(ui, |ui| {
                for m in TonemappingMode::ALL {
                    let enabled = caps.supports_hw_tonemap || !matches!(m, TonemappingMode::HardwareOnly | TonemappingMode::Both);
                    changed |= ui.add_enabled_ui(enabled, |ui| ui.selectable_value(&mut s.tonemapping, m, m.label())).inner.changed();
                }
            });
        changed |= ui.checkbox(&mut s.hdr_mode, "HDR mode (treat HDR input as HDR)").changed();
        if caps.supports_hdr_recording {
            changed |= ui.checkbox(&mut s.record_hdr, "Keep HDR (passthrough) in Auto").changed();
        }
        if changed {
            self.settings.tonemapping = s.tonemapping;
            self.settings.hdr_mode = s.hdr_mode;
            self.settings.record_hdr = s.record_hdr;
            self.apply_tonemap();
        }
        let hdr = self.input_is_hdr();
        let eff = tonemap::determine_state(hdr, self.settings.hdr_mode, self.settings.record_hdr, self.settings.tonemapping);
        ui.label(format!(
            "Input is {}: {}",
            if hdr { "HDR" } else { "SDR" },
            match (hdr, eff.hardware && caps.supports_hw_tonemap, eff.software || (eff.hardware && !caps.supports_hw_tonemap)) {
                (false, _, _) => "no tonemapping needed",
                (true, true, _) => "card tonemaps to SDR",
                (true, false, true) => "driver tonemaps to SDR on the CPU",
                (true, false, false) => "HDR passthrough (BT.2020 PQ, RGB 4:4:4)",
            }
        ));
        if let Some(want) = self.desired_tonemap_params() {
            let mismatch = want.iter().any(|(n, v)| driver::read_param(n).map(|c| c != *v).unwrap_or(false));
            if mismatch && ui.button("Apply to driver").on_hover_text("Driver parameters differ from this setting").clicked() {
                self.apply_tonemap();
            }
        }
        if !caps.supports_hw_tonemap {
            ui.small("Hardware tonemapping is only implemented for the 4K60 Pro MK.2 in the Linux driver.");
        }
    }

    fn edid_section(&mut self, ui: &mut egui::Ui, caps: &device::Capabilities) {
        let mut note: Option<String> = None;
        let mut reread = false;
        if let Some(ex) = self.extras.as_mut() {
            if caps.supports_edid_mode {
                if let (Some(ctrl), Some(d)) = (ex.edid_ctrl.clone(), ex.info_dev.as_ref()) {
                    let mut v = ex.edid_source.unwrap_or(ctrl.default_value);
                    let label = |v: i32| ctrl.menu.iter().find(|m| m.0 == v).map(|m| m.1.clone()).unwrap_or_else(|| v.to_string());
                    egui::ComboBox::from_label("EDID mode")
                        .selected_text(label(v))
                        .show_ui(ui, |ui| {
                            for (i, n) in &ctrl.menu {
                                ui.selectable_value(&mut v, *i, match n.as_str() {
                                    "Internal" => "Internal (card EDID)",
                                    "Display" => "Display (pass-through monitor)",
                                    "Merged" => "Merged (card + monitor)",
                                    o => o,
                                });
                            }
                        });
                    if Some(v) != ex.edid_source {
                        match d.set_ctrl(ctrl.id, v) {
                            Ok(()) => {
                                ex.edid_source = Some(v);
                                note = Some(format!("EDID mode set to {}; the source will re-negotiate", label(v)));
                            }
                            Err(e) => note = Some(format!("Setting EDID mode failed: {e}")),
                        }
                    }
                } else {
                    ui.small("This driver build has no EDID Source control.");
                }
            }
            if caps.supports_custom_edid {
                ui.separator();
                match &ex.edid {
                    Some(Ok(s)) => {
                        ui.label(format!("Current EDID: {} {} ({}, {})", s.manufacturer, s.name.clone().unwrap_or_default(), s.year, if s.rgb_only { "RGB only" } else { "RGB + YCbCr" }));
                        if let Some((w, h, r)) = s.preferred {
                            ui.label(format!("Preferred mode: {w}×{h} @ {r} Hz, {} extension block(s){}", s.extensions, if s.hdr_static_metadata { ", HDR" } else { "" }));
                        }
                    }
                    Some(Err(e)) => {
                        ui.colored_label(Color32::LIGHT_RED, format!("EDID read failed: {e}"));
                    }
                    None => {}
                }
                ui.horizontal(|ui| {
                    if ui.button("Re-read").clicked() {
                        reread = true;
                    }
                    if ui.button("Save to file").clicked() {
                        if let Some(d) = ex.info_dev.as_ref() {
                            let p = config_dir().join(format!("edid-{}.bin", chrono::Local::now().format("%Y%m%d-%H%M%S")));
                            note = Some(match d.get_edid().map_err(anyhow::Error::from).and_then(|b| {
                                std::fs::create_dir_all(config_dir())?;
                                std::fs::write(&p, b)?;
                                Ok(())
                            }) {
                                Ok(()) => format!("EDID saved to {}", p.display()),
                                Err(e) => format!("EDID save failed: {e:#}"),
                            });
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.edid_path).hint_text("/path/to/edid.bin").desired_width(220.0));
                    if ui.button("Write EDID").clicked() {
                        note = Some(write_custom_edid(ex, &self.edid_path));
                        reread = true;
                    }
                });
                let backup = config_dir().join("edid-backup.bin");
                if backup.exists() && ui.button("Restore original EDID").clicked() {
                    note = Some(write_custom_edid(ex, &backup.to_string_lossy()));
                    reread = true;
                }
                ui.small("The original EDID is backed up automatically before the first write. MK.2 writes are runtime-only; the 4K Pro stores them in its EEPROM.");
            }
        }
        if reread {
            self.read_edid();
        }
        if let Some(n) = note {
            self.note(n);
        }
    }

    fn audio_section(&mut self, ui: &mut egui::Ui, dev: &DeviceInfo) {
        match dev.alsa_card {
            Some(c) => {
                ui.label(format!("ALSA card {c} (plughw:{c})"));
            }
            None => {
                ui.label("No matching ALSA capture device.");
                return;
            }
        }
        let mut s = self.settings.clone();
        let mut changed = ui.checkbox(&mut s.audio_monitor, "Monitor audio").changed();
        changed |= ui.add(egui::Slider::new(&mut s.monitor_volume, 0.0..=2.0).text("Monitor volume")).drag_stopped();
        if changed {
            self.settings.audio_monitor = s.audio_monitor;
            self.settings.monitor_volume = s.monitor_volume;
            self.settings.save();
            self.update_monitor();
        } else {
            self.settings.monitor_volume = s.monitor_volume;
        }
        if dev.is_sc0710() {
            ui.small("The sc0710 driver only produces audio while video is streaming (unless keep_audio_alive=1).");
        }
    }

    fn recording_section(&mut self, ui: &mut egui::Ui) {
        if !self.gst_ok {
            ui.colored_label(Color32::LIGHT_RED, "gst-launch-1.0 not found; install GStreamer to record.");
            return;
        }
        let mut s = self.settings.clone();
        let mut dir = s.record_dir.to_string_lossy().into_owned();
        ui.horizontal(|ui| {
            ui.label("Folder");
            if ui.text_edit_singleline(&mut dir).lost_focus() {
                s.record_dir = PathBuf::from(&dir);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Container");
            ui.selectable_value(&mut s.container, Container::Mkv, "MKV");
            ui.selectable_value(&mut s.container, Container::Mp4, "MP4");
        });
        egui::ComboBox::from_label("Video encoder")
            .selected_text(self.video_encoders.iter().find(|e| e.id == s.video_encoder).map(|e| e.label).unwrap_or("none"))
            .show_ui(ui, |ui| {
                for e in &self.video_encoders {
                    ui.selectable_value(&mut s.video_encoder, e.id.to_string(), e.label);
                }
            });
        ui.add(egui::Slider::new(&mut s.video_bitrate_kbps, 2_000..=150_000).text("Video kbps").logarithmic(true));
        egui::ComboBox::from_label("Audio encoder")
            .selected_text(self.audio_encoders.iter().find(|e| e.id == s.audio_encoder).map(|e| e.label).unwrap_or("none (no audio)"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut s.audio_encoder, String::new(), "none (no audio)");
                for e in &self.audio_encoders {
                    ui.selectable_value(&mut s.audio_encoder, e.id.to_string(), e.label);
                }
            });
        ui.add(egui::Slider::new(&mut s.audio_bitrate_kbps, 64..=320).text("Audio kbps"));
        let changed = s.record_dir != self.settings.record_dir
            || s.container != self.settings.container
            || s.video_encoder != self.settings.video_encoder
            || s.video_bitrate_kbps != self.settings.video_bitrate_kbps
            || s.audio_encoder != self.settings.audio_encoder
            || s.audio_bitrate_kbps != self.settings.audio_bitrate_kbps;
        if changed {
            self.settings = s;
            self.settings.save();
        }
        if self.recording() {
            ui.small("Changes apply to the next recording.");
        }
    }

    fn driver_section(&mut self, ui: &mut egui::Ui) {
        if ui.button("Refresh").clicked() {
            self.refresh_driver_params();
        }
        egui::Grid::new("params").num_columns(2).striped(true).show(ui, |ui| {
            for (n, v) in &self.driver_params {
                ui.monospace(*n);
                ui.monospace(v.map(|v| v.to_string()).unwrap_or_else(|| "—".into()));
                ui.end_row();
            }
        });
        if let Some(p) = self.proc_for_current() {
            egui::CollapsingHeader::new("/proc/sc0710-state").show(ui, |ui| {
                ui.monospace(&p.raw);
            });
        }
        ui.small("Parameter writes need root: a pkexec prompt appears unless the udev rule from contrib/ is installed.");
    }
}

fn write_custom_edid(ex: &mut DeviceExtras, path: &str) -> String {
    let Some(d) = ex.info_dev.as_ref() else { return "Device not open".into() };
    let data = match std::fs::read(path.trim()) {
        Ok(b) => b,
        Err(e) => return format!("Cannot read {path}: {e}"),
    };
    if let Err(e) = edid::validate(&data) {
        return format!("Not a valid EDID: {e:#}");
    }
    let backup = config_dir().join("edid-backup.bin");
    if !backup.exists() {
        if let Ok(cur) = d.get_edid() {
            let _ = std::fs::create_dir_all(config_dir());
            let _ = std::fs::write(&backup, cur);
        }
    }
    match d.set_edid(&data) {
        Ok(()) => "EDID written; the source re-negotiates in ~10 s".into(),
        Err(e) => format!("EDID write failed: {e}"),
    }
}

impl App {
    /// Dev aid: `SC0710_STUDIO_UI_SHOT=/path.png` saves a window screenshot
    /// after a few seconds and quits.
    fn dev_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = std::env::var_os("SC0710_STUDIO_UI_SHOT") else { return };
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        static REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let start = *START.get_or_init(Instant::now);
        if start.elapsed() > Duration::from_secs(4) && !REQUESTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let [w, h] = img.size;
            let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Ok(f) = std::fs::File::create(&path) {
                let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
                enc.set_color(png::ColorType::Rgba);
                if let Ok(mut wr) = enc.write_header() {
                    let _ = wr.write_image_data(&rgba);
                }
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.dev_screenshot(ctx);
        if self.last_scan.elapsed() > Duration::from_secs(3) {
            self.rescan(ctx);
        }
        if self.last_poll.elapsed() > Duration::from_millis(500) {
            self.poll_info();
            // The recorder takes over the ALSA device; restart the monitor afterwards.
            self.update_monitor();
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| self.top_bar(ui, ctx));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_bar(ui));
        egui::SidePanel::right("settings").default_width(380.0).resizable(true).show(ctx, |ui| self.side_panel(ui));
        egui::CentralPanel::default().frame(egui::Frame::default().fill(Color32::BLACK)).show(ctx, |ui| self.preview(ui, ctx));
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.settings.save();
        self.monitor = None;
        self.capture = None;
    }
}
