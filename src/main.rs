//! Meeting Recorder: records a meeting in two tracks (mic and computer
//! audio), transcribes it with whisper.cpp after the call, and streams live
//! levels to a bar widget.

mod actions;
mod agent;
mod animation;
mod audio;
mod bar_widget;
mod chapters;
mod diarize;
mod export;
mod ipc;
mod meeting;
mod models;
mod nemotron;
mod player;
mod settings;
mod notes;
mod theme;
mod transcribe;
mod ui;
mod webcam;

use gtk::glib;

pub const APP_ID: &str = "com.jankeesvw.OmarchyMeetingRecorder";
pub const APP_NAME: &str = "omarchy-meeting-recorder";

/// The meeting's name in `start`'s arguments: what follows `--name`, or for
/// older scripts the arguments themselves.
fn name_argument(args: &[String]) -> Vec<String> {
    match args.iter().position(|arg| arg == "--name") {
        Some(at) => args[at + 1..].to_vec(),
        None => args.to_vec(),
    }
}

/// Sends `lines` to the running app, or opens the app and sends them as soon as it listens.
fn send_or_open(lines: Vec<String>) -> glib::ExitCode {
    let Some(first) = lines.first().cloned() else {
        return glib::ExitCode::SUCCESS;
    };
    if ipc::send(&first) {
        lines[1..].iter().for_each(|line| {
            ipc::send(line);
        });
        return glib::ExitCode::SUCCESS;
    }
    std::thread::spawn(move || {
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if ipc::send(&first) {
                lines[1..].iter().for_each(|line| {
                    ipc::send(line);
                });
                return;
            }
        }
    });
    ui::run(None)
}

fn main() -> glib::ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => ui::run(None),
        Some("--version" | "-V") => {
            let backend = if cfg!(feature = "vulkan") {
                "Vulkan support; CPU fallback"
            } else {
                "CPU-only"
            };
            println!("{APP_NAME} {} ({backend})", env!("CARGO_PKG_VERSION"));
            glib::ExitCode::SUCCESS
        }
        Some("watch") => {
            ipc::watch();
            glib::ExitCode::SUCCESS
        }
        Some("start") => send_or_open(vec![ipc::start_line(&name_argument(
            &std::env::args().skip(2).collect::<Vec<_>>(),
        ))]),
        Some(command @ ("stop" | "compact" | "pause")) => {
            if ipc::send(command) {
                glib::ExitCode::SUCCESS
            } else {
                eprintln!("{APP_NAME}: the recorder is not running");
                glib::ExitCode::FAILURE
            }
        }
        Some("wait") => {
            if ipc::wait(std::env::args().any(|arg| arg == "--audio")) {
                glib::ExitCode::SUCCESS
            } else {
                eprintln!("{APP_NAME}: the recorder closed before the meeting was done");
                glib::ExitCode::FAILURE
            }
        }
        // Only from the command line: there is no button for it in the app.
        Some("notes") => {
            // notes <file> [--name <name>]: the name goes to the meeting about to be recorded.
            let args: Vec<String> = std::env::args().skip(2).collect();
            let (file, name) = match args.iter().position(|arg| arg == "--name") {
                Some(at) => (args[..at].first().cloned(), args[at + 1..].to_vec()),
                None => (args.first().cloned(), Vec::new()),
            };
            let line = file
                .and_then(|path| std::fs::canonicalize(path).ok())
                .and_then(|path| ipc::notes_line(&path));
            match line {
                Some(line) => send_or_open([Some(line), ipc::name_line(&name)].into_iter().flatten().collect()),
                None => {
                    eprintln!("{APP_NAME}: notes needs a text file that exists");
                    glib::ExitCode::from(2)
                }
            }
        }
        Some("transcribe-file") => {
            transcribe::cli_file(&std::env::args().skip(2).collect::<Vec<_>>())
        }
        Some("diarize") => diarize::cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        Some("transcribe") => transcribe::cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        Some("action") => actions::cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        // A new window in the running app, or the app itself when it is not running.
        Some("new-window") => {
            if ipc::send("new-window") {
                glib::ExitCode::SUCCESS
            } else {
                ui::run(None)
            }
        }
        Some("ask") => agent::cli(&std::env::args().skip(2).collect::<Vec<_>>()),
        Some("-h" | "--help") => {
            println!(
                "Usage: {APP_NAME} [start [--name <name>] | stop | pause | compact | watch | wait [--audio] | notes <file> [--name <name>] | transcribe <mic> <computer> [--language xx]]"
            );
            println!();
            println!("  (no command)  open the recorder, ready to record");
            println!("  <meeting>     open a .meeting-recorder file or a meeting folder");
            println!("  start [--name <name>]  start recording, opening the recorder if needed");
            println!("  stop          stop the running recording (for a keybinding)");
            println!("  compact       switch the recording window between full and compact");
            println!("  pause         pause or resume the running recording");
            println!("  watch         stream the recorder state as NDJSON, for the bar widget");
            println!(
                "  wait          wait until the recording is done and print its folder (--audio: once the audio is saved)"
            );
            println!(
                "  notes <file> [--name <name>]  show notes next to the recorder, to read from and edit while recording, and name the meeting about to be recorded"
            );
            println!("  transcribe    transcribe two tracks and print the transcript as Markdown");
            println!(
                "  ask           run a prompt over stdin through the default agent, without tools"
            );
            println!("  action        run one of your actions on a meeting folder");
            println!(
                "  new-window    open another window, for a second meeting (Ctrl+N in the app)"
            );
            glib::ExitCode::SUCCESS
        }
        Some(path)
            if path.ends_with(&format!(".{}", meeting::EXTENSION))
                || std::path::Path::new(path).is_dir() =>
        {
            ui::run(Some(path))
        }
        Some(other) => {
            eprintln!("{APP_NAME}: unknown command '{other}', see --help");
            glib::ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::name_argument;

    #[test]
    fn start_takes_its_name_after_the_flag_or_as_before() {
        let args = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert_eq!(name_argument(&args(&["--name", "Voice-over", "prioriteit"])), args(&["Voice-over", "prioriteit"]));
        assert_eq!(name_argument(&args(&["Weekly", "sync"])), args(&["Weekly", "sync"]));
        assert!(name_argument(&[]).is_empty());
    }
}
