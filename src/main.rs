mod app;
mod audio;
mod capture;
mod convert;
mod device;
mod driver;
mod edid;
mod recorder;
mod settings;
mod tonemap;
mod v4l2;

use eframe::egui;

fn print_devices() {
    let devs = device::discover(true);
    if devs.is_empty() {
        println!("no V4L2 capture devices found");
    }
    for d in devs {
        let caps = d.board.capabilities();
        println!(
            "{}  driver={} card={:?} board={} alsa={}",
            d.video_path.display(),
            d.driver,
            d.card_name,
            if d.is_sc0710() { caps.name } else { "-" },
            d.alsa_card.map(|c| format!("hw:{c}")).unwrap_or_else(|| "-".into())
        );
        if let Ok(dev) = v4l2::Device::open(&d.video_path) {
            let fmts: Vec<String> = dev.enum_formats().iter().map(|(f, n)| format!("{} ({n})", v4l2::fourcc_str(*f))).collect();
            println!("    formats: {}", fmts.join(", "));
            if let Ok(f) = dev.get_format() {
                println!("    current: {}x{} {} stride {}", f.width, f.height, v4l2::fourcc_str(f.pixelformat), f.bytesperline);
            }
            match dev.query_dv_timings() {
                Ok(t) => println!("    signal: {}x{}{} @ {:.2} Hz", t.width, t.height, if t.interlaced { "i" } else { "p" }, t.fps()),
                Err(e) => println!("    signal: {e}"),
            }
            for c in dev.controls() {
                println!("    control {:#010x} {:?} [{}..{}] default {}", c.id, c.name, c.minimum, c.maximum, c.default_value);
            }
        }
    }
    for (p, sd, bound) in device::sc0710_pci_functions() {
        println!("PCI {} subsystem 1cfa:{sd:04x} bound={bound}", p.display());
    }
}

/// Headless capture + record for `secs` seconds using the saved settings.
fn record_headless(device: Option<&str>, secs: u64) -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    let s = settings::Settings::load();
    let devs = device::discover(true);
    let dev = match device {
        Some(p) => devs.iter().find(|d| d.video_path.to_string_lossy() == p),
        None => devs.iter().find(|d| d.is_sc0710()).or(devs.first()),
    }
    .ok_or_else(|| anyhow::anyhow!("no capture device found"))?
    .clone();
    println!("recording {secs}s from {}", dev.label());
    let venc = recorder::available_video_encoders();
    let aenc = recorder::available_audio_encoders();
    let pick = |want: &str, list: &[recorder::EncoderInfo]| {
        list.iter().find(|e| e.id == want).or(list.first()).map(|e| e.id.to_string())
    };
    let handle = capture::start(
        dev.video_path.clone(),
        capture::StartOptions {
            pixel_format: s.pixel_format,
            range: s.color_range,
            picture: s.picture,
            preview_width: 640,
            repaint: Box::new(|| {}),
        },
    );
    std::thread::sleep(Duration::from_millis(1500));
    if let Some(p) = std::env::var_os("SC0710_STUDIO_SNAPSHOT") {
        handle.send(capture::Command::Snapshot(p.into()));
        std::thread::sleep(Duration::from_millis(1500));
        for m in handle.shared.status.lock().unwrap().messages.drain(..) {
            println!("{m}");
        }
        return Ok(());
    }
    if std::env::var_os("SC0710_STUDIO_NOREC").is_some() {
        for _ in 0..secs {
            std::thread::sleep(Duration::from_secs(1));
            let st = handle.shared.status.lock().unwrap().clone();
            println!("{:.1} fps frames={} streaming={} err={:?}", st.measured_fps, st.frames, st.streaming, st.error);
        }
        return Ok(());
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let cfg = recorder::RecordConfig {
        path: std::env::var_os("SC0710_STUDIO_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| s.record_dir.clone())
            .join(format!("capture_{stamp}.{}", s.container.ext())),
        container: s.container,
        video_encoder: pick(&s.video_encoder, &venc).ok_or_else(|| anyhow::anyhow!("no video encoder"))?,
        video_kbps: s.video_bitrate_kbps,
        audio_encoder: pick(&s.audio_encoder, &aenc),
        audio_kbps: s.audio_bitrate_kbps,
        alsa_card: dev.alsa_card,
        monitor_volume: None,
    };
    handle.send(capture::Command::StartRecording(cfg, false));
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let st = handle.shared.status.lock().unwrap().clone();
        let fmt = st.format.map(|f| format!("{}x{} {}", f.width, f.height, v4l2::fourcc_str(f.fourcc))).unwrap_or_default();
        let rec = st.recording.map(|r| format!("written {} dropped {}", r.written, r.dropped)).unwrap_or_else(|| "not recording".into());
        println!("{fmt} {:.1} fps, {rec} {}", st.measured_fps, st.error.unwrap_or_default());
        for m in st.messages {
            println!("  {m}");
        }
    }
    handle.send(capture::Command::StopRecording);
    std::thread::sleep(Duration::from_millis(500));
    drop(handle);
    Ok(())
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "sc0710-studio [--all-devices] [--list] [--record SECS [--device /dev/videoN]]\n  --all-devices  also list non-sc0710 capture devices (e.g. UVC)\n  --list         print detected devices and exit\n  --record SECS  record without the GUI using the saved settings"
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--list") {
        print_devices();
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--record") {
        let secs = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(10);
        let dev = args.iter().position(|a| a == "--device").and_then(|j| args.get(j + 1)).map(String::as_str);
        if let Err(e) = record_headless(dev, secs) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let show_all = args.iter().any(|a| a == "--all-devices");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 860.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("Ligato Studio")
            .with_app_id("sc0710-studio"),
        ..Default::default()
    };
    eframe::run_native("Ligato Studio", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, show_all)))))
}
