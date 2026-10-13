//! The webcam, recorded next to the audio when one is picked.
//!
//! ffmpeg takes the camera and the microphone, one segment for every stretch
//! between pauses, and the segments become `webcam.mp4` in the meeting
//! folder when it is saved. The microphone in the video is there so an
//! editor can line the picture up with the audio tracks exactly: the camera
//! always starts a moment after the audio.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
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

/// The camera the webcam toggle records: the one named `saved` when it is
/// there, else the first that is not a dummy loopback device.
pub fn pick(devices: &[(PathBuf, String)], saved: &str) -> Option<(PathBuf, String)> {
    devices
        .iter()
        .find(|(_, name)| !saved.is_empty() && name == saved)
        .or_else(|| devices.iter().find(|(_, name)| !name.starts_with("Dummy video device")))
        .cloned()
}

fn segment(staging: &Path, n: u32) -> PathBuf {
    staging.join(format!("webcam-{n:03}.mkv"))
}

/// The preview: a small picture, a dozen frames a second, as JPEGs on stdout.
const PREVIEW: [&str; 10] = [
    "-vf", "scale=192:-2,fps=12", "-c:v", "mjpeg", "-q:v", "7", "-f", "image2pipe", "pipe:1", "-y",
];

/// An ffmpeg that stops with the app, should the app go away mid-recording,
/// instead of filling the disk or keeping the camera on.
fn ffmpeg() -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostats"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    command
}

/// Takes the first whole JPEG off the front of `buffer`, dropping what came before it.
fn take_jpeg(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let start = buffer.windows(2).position(|w| w == [0xFF, 0xD8])?;
    let end = buffer[start + 2..].windows(2).position(|w| w == [0xFF, 0xD9])? + start + 4;
    let frame = buffer[start..end].to_vec();
    buffer.drain(..end);
    Some(frame)
}

/// Hands the frames a process writes to the preview, until its stdout closes.
/// Frames the window has no time for are dropped.
fn read_frames(child: &mut Child, frames: &async_channel::Sender<Vec<u8>>) {
    let Some(mut stdout) = child.stdout.take() else { return };
    let frames = frames.clone();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let read = match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            buffer.extend_from_slice(&chunk[..read]);
            while let Some(frame) = take_jpeg(&mut buffer) {
                let _ = frames.try_send(frame);
            }
        }
    });
}

pub struct Webcam {
    device: RefCell<Option<PathBuf>>,
    staging: RefCell<Option<PathBuf>>,
    running: RefCell<Option<Child>>,
    /// Segments told to stop that may still be writing their last frames.
    ending: RefCell<Vec<Child>>,
    segments: Cell<u32>,
    /// The camera shown, before a recording starts.
    previewing: RefCell<Option<Child>>,
    frames: async_channel::Sender<Vec<u8>>,
}

impl Webcam {
    /// A webcam whose preview frames, JPEGs, go to `frames`.
    pub fn new(frames: async_channel::Sender<Vec<u8>>) -> Self {
        Webcam {
            device: RefCell::default(),
            staging: RefCell::default(),
            running: RefCell::default(),
            ending: RefCell::default(),
            segments: Cell::new(0),
            previewing: RefCell::default(),
            frames,
        }
    }

    /// Shows `device` in the preview while nothing records, or stops showing it with None.
    pub fn preview(&self, device: Option<&Path>) {
        let Some(device) = device else {
            self.stop_preview();
            return;
        };
        if self.previewing.borrow().is_some() || self.running.borrow().is_some() {
            return;
        }
        let mut command = ffmpeg();
        command.args(["-f", "v4l2", "-i"]).arg(device).args(PREVIEW).stdin(Stdio::null());
        match command.spawn() {
            Ok(mut child) => {
                read_frames(&mut child, &self.frames);
                *self.previewing.borrow_mut() = Some(child);
            }
            Err(err) => eprintln!("{APP_NAME}: could not show the webcam: {err}"),
        }
    }

    /// A camera opens for one program at a time: the preview lets go before recording.
    fn stop_preview(&self) {
        if let Some(mut child) = self.previewing.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Starts recording `device` into the staging folder of a new recording.
    pub fn start(&self, device: &Path, staging: &Path) {
        *self.device.borrow_mut() = Some(device.to_owned());
        *self.staging.borrow_mut() = Some(staging.to_owned());
        self.segments.set(0);
        self.resume();
    }

    /// Starts the next segment, after a pause or at the start. The same
    /// ffmpeg feeds the preview.
    pub fn resume(&self) {
        let (Some(device), Some(staging)) = (self.device.borrow().clone(), self.staging.borrow().clone())
        else {
            return;
        };
        self.stop_preview();
        let n = self.segments.get();
        self.segments.set(n + 1);
        let mut command = ffmpeg();
        command
            .args(["-thread_queue_size", "512", "-f", "v4l2", "-i"])
            .arg(&device)
            .args(["-thread_queue_size", "512", "-f", "pulse", "-i", "default"])
            .args(["-map", "0:v", "-map", "1:a"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p"])
            .args(["-c:a", "aac", "-b:a", "160k", "-y"])
            .arg(segment(&staging, n))
            .args(["-map", "0:v"])
            .args(PREVIEW)
            .stdin(Stdio::piped());
        match command.spawn() {
            Ok(mut child) => {
                read_frames(&mut child, &self.frames);
                *self.running.borrow_mut() = Some(child);
            }
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

impl Drop for Webcam {
    /// A closed window turns its camera off.
    fn drop(&mut self) {
        self.stop_preview();
        for mut child in self.running.take().into_iter().chain(self.ending.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
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
    fn the_toggle_records_a_real_camera() {
        let devices = vec![
            (PathBuf::from("/dev/video0"), "Dummy video device (0x0000)".to_owned()),
            (PathBuf::from("/dev/video9"), "Opal C1".to_owned()),
            (PathBuf::from("/dev/video2"), "Laptop camera".to_owned()),
        ];
        assert_eq!(pick(&devices, "").map(|d| d.1), Some("Opal C1".to_owned()));
        assert_eq!(pick(&devices, "Laptop camera").map(|d| d.1), Some("Laptop camera".to_owned()));
        assert_eq!(pick(&devices, "Gone").map(|d| d.1), Some("Opal C1".to_owned()));
        assert_eq!(pick(&devices[..1], ""), None);
    }

    #[test]
    fn frames_are_cut_out_of_the_stream() {
        let mut stream = vec![0x00, 0xFF, 0xD8, 1, 2, 0xFF, 0xD9, 0xFF, 0xD8, 3];
        assert_eq!(take_jpeg(&mut stream), Some(vec![0xFF, 0xD8, 1, 2, 0xFF, 0xD9]));
        // The next frame is not whole yet: it waits for more of the stream.
        assert_eq!(take_jpeg(&mut stream), None);
        assert_eq!(stream, vec![0xFF, 0xD8, 3]);
    }

    #[test]
    fn segments_are_numbered_in_order() {
        let staging = Path::new("/tmp/rec");
        assert_eq!(segment(staging, 0), Path::new("/tmp/rec/webcam-000.mkv"));
        assert_eq!(segment(staging, 12), Path::new("/tmp/rec/webcam-012.mkv"));
    }
}
