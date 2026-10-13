//! The webcam, recorded next to the audio when one is picked.
//!
//! ffmpeg takes the camera and the microphone, one segment for every stretch
//! between pauses, and the segments become `webcam.mp4` in the meeting
//! folder when it is saved. The microphone in the video is there so an
//! editor can line the picture up with the audio tracks exactly: the camera
//! always starts a moment after the audio.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::APP_NAME;

/// The video in a meeting folder, when a webcam was recorded.
pub const FILE: &str = "webcam.mp4";

/// The video devices, as (device, name), sorted by name.
pub fn devices() -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = std::fs::read_to_string(entry.path().join("name")).ok()?;
            Some((PathBuf::from("/dev").join(entry.file_name()), name.trim().to_owned()))
        })
        .collect();
    found.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    found
}

fn segment(staging: &Path, n: u32) -> PathBuf {
    staging.join(format!("webcam-{n:03}.mkv"))
}

#[derive(Default)]
pub struct Webcam {
    device: RefCell<Option<PathBuf>>,
    staging: RefCell<Option<PathBuf>>,
    running: RefCell<Option<Child>>,
    /// Segments told to stop that may still be writing their last frames.
    ending: RefCell<Vec<Child>>,
    segments: Cell<u32>,
}

impl Webcam {
    /// Starts recording `device` into the staging folder of a new recording.
    pub fn start(&self, device: &Path, staging: &Path) {
        *self.device.borrow_mut() = Some(device.to_owned());
        *self.staging.borrow_mut() = Some(staging.to_owned());
        self.segments.set(0);
        self.resume();
    }

    /// Starts the next segment, after a pause or at the start.
    pub fn resume(&self) {
        let (Some(device), Some(staging)) = (self.device.borrow().clone(), self.staging.borrow().clone())
        else {
            return;
        };
        let n = self.segments.get();
        self.segments.set(n + 1);
        let mut command = Command::new("ffmpeg");
        command
            .args(["-hide_banner", "-loglevel", "error", "-nostats"])
            .args(["-thread_queue_size", "512", "-f", "v4l2", "-i"])
            .arg(&device)
            .args(["-thread_queue_size", "512", "-f", "pulse", "-i", "default"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p"])
            .args(["-c:a", "aac", "-b:a", "160k", "-y"])
            .arg(segment(&staging, n))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Should the app go away mid-recording, the camera stops with it
        // instead of filling the disk.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match command.spawn() {
            Ok(child) => *self.running.borrow_mut() = Some(child),
            Err(err) => eprintln!("{APP_NAME}: could not start the webcam: {err}"),
        }
    }

    /// Ends the running segment: ffmpeg closes the file once it reads q.
    pub fn pause(&self) {
        if let Some(mut child) = self.running.borrow_mut().take() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"q");
            }
            self.ending.borrow_mut().push(child);
        }
    }

    /// Ends the recording; the segments that may still be closing go to `export`.
    pub fn stop(&self) -> Vec<Child> {
        self.pause();
        *self.device.borrow_mut() = None;
        std::mem::take(&mut *self.ending.borrow_mut())
    }
}

/// Waits for the last segments to close and joins all of them into
/// `webcam.mp4` in `out`. Blocking. False when there was nothing to join.
pub fn export(staging: &Path, out: &Path, closing: Vec<Child>) -> bool {
    for mut child in closing {
        let deadline = Instant::now() + Duration::from_secs(10);
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let parts: Vec<PathBuf> = (0..)
        .map(|n| segment(staging, n))
        .take_while(|path| path.exists())
        .filter(|path| std::fs::metadata(path).is_ok_and(|m| m.len() > 0))
        .collect();
    if parts.is_empty() {
        return false;
    }
    let list = staging.join("webcam.txt");
    let lines: String = parts
        .iter()
        .map(|path| format!("file '{}'\n", path.display().to_string().replace('\'', "'\\''")))
        .collect();
    if std::fs::write(&list, lines).is_err() {
        return false;
    }
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-c", "copy", "-movflags", "+faststart", "-y"])
        .arg(out.join(FILE))
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_numbered_in_order() {
        let staging = Path::new("/tmp/rec");
        assert_eq!(segment(staging, 0), Path::new("/tmp/rec/webcam-000.mkv"));
        assert_eq!(segment(staging, 12), Path::new("/tmp/rec/webcam-012.mkv"));
    }
}
