//! Live audio monitoring of the card's ALSA capture PCM via gst-launch-1.0.

use std::process::{Child, Command, Stdio};

pub struct AudioMonitor {
    child: Child,
    pub card: u32,
    pub volume: f32,
}

impl AudioMonitor {
    pub fn start(card: u32, volume: f32) -> std::io::Result<AudioMonitor> {
        let child = Command::new("gst-launch-1.0")
            .args([
                "-q",
                "alsasrc",
                &format!("device=plughw:{card}"),
                "buffer-time=40000",
                "!",
                "queue",
                "leaky=downstream",
                "max-size-time=100000000",
                "!",
                "audioconvert",
                "!",
                "audioresample",
                "!",
                "volume",
                &format!("volume={volume:.2}"),
                "!",
                "autoaudiosink",
                "sync=false",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(AudioMonitor { child, card, volume })
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for AudioMonitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
