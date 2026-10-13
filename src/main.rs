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
mod teleprompter;
mod theme;
mod transcribe;
mod ui;

use gtk::glib;

pub const APP_ID: &str = "com.jankeesvw.OmarchyMeetingRecorder";
pub const APP_NAME: &str = "omarchy-meeting-recorder";

/// Sends `line` to the running app, or opens the app and sends it as soon as it listens.
fn send_or_open(line: String) -> glib::ExitCode {
    if ipc::send(&line) {
        return glib::ExitCode::SUCCESS;
    }
    std::thread::spawn(move || {
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if ipc::send(&line) {
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
        Some("start") => {
            send_or_open(ipc::start_line(&std::env::args().skip(2).collect::<Vec<_>>()))
        }
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
        Some("teleprompter") => {
            let line = std::env::args()
                .nth(2)
                .and_then(|path| std::fs::canonicalize(path).ok())
                .and_then(|path| ipc::teleprompter_line(&path));
            match line {
                Some(line) => send_or_open(line),
                None => {
                    eprintln!("{APP_NAME}: teleprompter needs a text file that exists");
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
                "Usage: {APP_NAME} [start [name] | stop | pause | compact | watch | wait [--audio] | teleprompter <file> | transcribe <mic> <computer> [--language xx]]"
            );
            println!();
            println!("  (no command)  open the recorder, ready to record");
            println!("  <meeting>     open a .meeting-recorder file or a meeting folder");
            println!("  start [name]  start recording, opening the recorder if needed");
            println!("  stop          stop the running recording (for a keybinding)");
            println!("  compact       switch the recording window between full and compact");
            println!("  pause         pause or resume the running recording");
            println!("  watch         stream the recorder state as NDJSON, for the bar widget");
            println!(
                "  wait          wait until the recording is done and print its folder (--audio: once the audio is saved)"
            );
            println!(
                "  teleprompter  show a script next to the recorder, to read and edit while recording"
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
