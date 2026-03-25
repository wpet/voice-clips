// voice-clip: Background daemon for WSL2 audio capture + OpenAI Whisper transcription
//
// Usage:
//   voice-clip daemon   — run background service (listens on Unix socket)
//   voice-clip toggle   — toggle recording on/off
//   voice-clip status   — print current state
//   voice-clip stop     — gracefully stop daemon

use anyhow::{anyhow, Context, Result};
use chrono::Local;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{env, fs, process};

// ─── Constants ───────────────────────────────────────────────────────────────

const SOCKET_PATH: &str = "/tmp/voice-clip.sock";
const PID_PATH: &str = "/tmp/voice-clip.pid";
const REC_MARKER: &str = "/tmp/voice-clip.recording";
const DEFAULT_DEVICE: &str = "plughw:0,0";
const DEFAULT_LANG: &str = "nl";
const SAMPLE_RATE: u32 = 16000;
const CHANNELS: u32 = 1;
const WHISPER_URL: &str = "https://api.openai.com/v1/audio/transcriptions";

// ─── Configuration ───────────────────────────────────────────────────────────

struct Config {
    api_key: String,
    lang: String,
    device: String,
    clips_dir: PathBuf,
}

fn load_config() -> Result<Config> {
    let home = env::var("HOME").context("HOME not set")?;
    let env_path = PathBuf::from(&home).join("voice-clips/.env");

    let mut api_key = None;
    let mut lang = None;
    let mut device = None;

    if env_path.exists() {
        let contents = fs::read_to_string(&env_path)
            .with_context(|| format!("Failed to read {}", env_path.display()))?;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"').trim_matches('\'');
                match key {
                    "OPENAI_API_KEY" => api_key = Some(value.to_string()),
                    "VOICE_CLIP_LANG" => lang = Some(value.to_string()),
                    "VOICE_CLIP_DEVICE" => device = Some(value.to_string()),
                    _ => {}
                }
            }
        }
    }

    let api_key = api_key.ok_or_else(|| anyhow!("OPENAI_API_KEY not found in {}", env_path.display()))?;
    let clips_dir = PathBuf::from(&home).join("voice-clips/clips");
    fs::create_dir_all(&clips_dir).context("Failed to create clips directory")?;

    Ok(Config {
        api_key,
        lang: lang.unwrap_or_else(|| DEFAULT_LANG.to_string()),
        device: device.unwrap_or_else(|| DEFAULT_DEVICE.to_string()),
        clips_dir,
    })
}

// ─── Audio Recording (ALSA) ─────────────────────────────────────────────────

fn record_audio(device: &str, stop_flag: &Arc<AtomicBool>, wav_path: &Path) -> Result<()> {
    use alsa::pcm::{Access, Format, HwParams, State};
    use alsa::{Direction, PCM};

    let pcm = PCM::new(device, Direction::Capture, false)
        .with_context(|| format!("Failed to open ALSA device '{}'", device))?;

    // Configure hardware parameters
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(CHANNELS)?;
        hwp.set_rate(SAMPLE_RATE, alsa::ValueOr::Nearest)?;
        hwp.set_format(Format::s16())?;
        hwp.set_access(Access::RWInterleaved)?;
        // Buffer: 0.5s, period: 0.1s
        hwp.set_buffer_size(SAMPLE_RATE as i64 / 2)?;
        hwp.set_period_size((SAMPLE_RATE as i64) / 10, alsa::ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
    }

    pcm.start()?;

    // Set up WAV writer
    let spec = hound::WavSpec {
        channels: CHANNELS as u16,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(wav_path, spec)
        .with_context(|| format!("Failed to create WAV file: {}", wav_path.display()))?;

    // Read loop — 100ms chunks
    let frames_per_period = SAMPLE_RATE as usize / 10;
    let mut buf = vec![0i16; frames_per_period * CHANNELS as usize];
    let io = pcm.io_i16()?;

    while !stop_flag.load(Ordering::Relaxed) {
        match io.readi(&mut buf) {
            Ok(frames) => {
                for &sample in &buf[..frames * CHANNELS as usize] {
                    writer.write_sample(sample)?;
                }
            }
            Err(e) => {
                // Try to recover from xrun
                eprintln!("[voice-clip] ALSA read error: {}, recovering", e);
                if let Err(re) = pcm.recover(e.errno() as i32, true) {
                    eprintln!("[voice-clip] Recovery failed: {}", re);
                    break;
                }
                // After recovery, check state and restart if needed
                if pcm.state() != State::Running {
                    if let Err(se) = pcm.start() {
                        eprintln!("[voice-clip] Failed to restart after recovery: {}", se);
                        break;
                    }
                }
            }
        }
    }

    writer.finalize()?;

    // Stop and drop PCM
    drop(io);
    let _ = pcm.drain();

    Ok(())
}

// ─── Transcription (OpenAI Whisper API) ──────────────────────────────────────

async fn transcribe(api_key: &str, wav_path: &Path, lang: &str) -> Result<String> {
    let file_bytes = tokio::fs::read(wav_path).await
        .with_context(|| format!("Failed to read WAV file: {}", wav_path.display()))?;
    let file_name = wav_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let file_part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str("audio/wav")?;

    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", "whisper-1")
        .text("language", lang.to_string());

    let client = reqwest::Client::new();
    let resp = client
        .post(WHISPER_URL)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .context("Whisper API request failed")?;

    let status = resp.status();
    let body = resp.text().await.context("Failed to read Whisper response")?;

    if !status.is_success() {
        return Err(anyhow!("Whisper API error ({}): {}", status, body));
    }

    let json: serde_json::Value =
        serde_json::from_str(&body).context("Failed to parse Whisper JSON response")?;
    let text = json["text"]
        .as_str()
        .ok_or_else(|| anyhow!("No 'text' field in Whisper response: {}", body))?
        .to_string();

    Ok(text)
}

// ─── Clipboard (Windows clip.exe) ────────────────────────────────────────────

fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    let mut child = Command::new("/mnt/c/Windows/System32/clip.exe")
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to spawn clip.exe")?;

    if let Some(ref mut stdin) = child.stdin {
        stdin
            .write_all(text.as_bytes())
            .context("Failed to write to clip.exe stdin")?;
    }

    child.wait().context("clip.exe failed")?;
    Ok(())
}

// ─── Tmux Feedback ──────────────────────────────────────────────────────────

fn tmux_message(tmux_env: &Option<String>, msg: &str) {
    if let Some(tmux_val) = tmux_env {
        if !tmux_val.is_empty() {
            let _ = std::process::Command::new("tmux")
                .args(["display-message", msg])
                .status();
        }
    }
}

// ─── Daemon ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum DaemonState {
    Idle,
    Recording,
    Transcribing,
}

impl std::fmt::Display for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonState::Idle => write!(f, "idle"),
            DaemonState::Recording => write!(f, "recording"),
            DaemonState::Transcribing => write!(f, "transcribing"),
        }
    }
}

fn run_daemon() -> Result<()> {
    // Ignore SIGPIPE — client disconnect must not kill the daemon
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let config = load_config()?;

    // Clean up stale socket
    if Path::new(SOCKET_PATH).exists() {
        fs::remove_file(SOCKET_PATH).ok();
    }

    // Write PID file
    fs::write(PID_PATH, process::id().to_string()).context("Failed to write PID file")?;

    // Bind socket
    let listener =
        UnixListener::bind(SOCKET_PATH).context("Failed to bind Unix socket")?;
    listener
        .set_nonblocking(true)
        .context("Failed to set socket non-blocking")?;

    eprintln!("[voice-clip] Daemon started (PID {})", process::id());

    // Shared state
    let running = Arc::new(AtomicBool::new(true));
    let recording_flag = Arc::new(AtomicBool::new(false)); // signals recording thread to stop
    let state = Arc::new(std::sync::Mutex::new(DaemonState::Idle));

    // Handle for the recording thread
    let rec_handle: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>> =
        Arc::new(std::sync::Mutex::new(None));
    // Path of current recording
    let current_wav: Arc<std::sync::Mutex<Option<PathBuf>>> =
        Arc::new(std::sync::Mutex::new(None));
    // Tmux env from client
    let tmux_env: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));

    // Signal handling
    let running_sig = running.clone();
    let recording_flag_sig = recording_flag.clone();
    ctrlc_setup(running_sig, recording_flag_sig);

    // Tokio runtime for async transcription
    let rt = tokio::runtime::Runtime::new()?;

    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = handle_client(
                    stream,
                    &config,
                    &running,
                    &recording_flag,
                    &state,
                    &rec_handle,
                    &current_wav,
                    &tmux_env,
                    &rt,
                ) {
                    eprintln!("[voice-clip] Client error: {}", e);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No connection waiting — sleep briefly
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[voice-clip] Accept error: {}", e);
            }
        }
    }

    // Clean up
    eprintln!("[voice-clip] Shutting down...");
    recording_flag.store(true, Ordering::Relaxed);
    if let Ok(mut h) = rec_handle.lock() {
        if let Some(handle) = h.take() {
            let _ = handle.join();
        }
    }
    cleanup();
    eprintln!("[voice-clip] Daemon stopped.");
    Ok(())
}

fn handle_client(
    stream: UnixStream,
    config: &Config,
    running: &Arc<AtomicBool>,
    recording_flag: &Arc<AtomicBool>,
    state: &Arc<std::sync::Mutex<DaemonState>>,
    rec_handle: &Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>>,
    current_wav: &Arc<std::sync::Mutex<Option<PathBuf>>>,
    tmux_env: &Arc<std::sync::Mutex<Option<String>>>,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let line = line.trim();

    // Parse command; toggle may include tmux env: "toggle TMUX=..."
    let (cmd, client_tmux) = if line.starts_with("toggle") {
        let rest = line.strip_prefix("toggle").unwrap().trim();
        let tmux = if rest.starts_with("TMUX=") {
            Some(rest.strip_prefix("TMUX=").unwrap().to_string())
        } else if rest.is_empty() {
            None
        } else {
            None
        };
        ("toggle", tmux)
    } else {
        (line, None)
    };

    // Update stored tmux env if client provided one
    if let Some(t) = &client_tmux {
        *tmux_env.lock().unwrap() = Some(t.clone());
    }

    let mut writer = stream.try_clone()?;

    match cmd {
        "toggle" => {
            let current = *state.lock().unwrap();
            match current {
                DaemonState::Idle => {
                    // Start recording
                    let timestamp = Local::now().format("%Y-%m-%d_%H%M%S").to_string();
                    let wav_path = config.clips_dir.join(format!("{}.wav", timestamp));

                    *current_wav.lock().unwrap() = Some(wav_path.clone());
                    recording_flag.store(false, Ordering::Relaxed);
                    *state.lock().unwrap() = DaemonState::Recording;
                    // Marker file for tmux statusbar
                    let _ = fs::write(REC_MARKER, "");

                    let stop = recording_flag.clone();
                    let device = config.device.clone();
                    let handle = std::thread::spawn(move || {
                        if let Err(e) = record_audio(&device, &stop, &wav_path) {
                            eprintln!("[voice-clip] Recording error: {}", e);
                        }
                    });
                    *rec_handle.lock().unwrap() = Some(handle);

                    let msg = "\u{25cf} REC \u{2014} Alt+V om te stoppen";
                    let stored_tmux = tmux_env.lock().unwrap().clone();
                    tmux_message(&stored_tmux, msg);
                    writeln!(writer, "recording")?;
                }
                DaemonState::Recording => {
                    // Stop recording and transcribe
                    recording_flag.store(true, Ordering::Relaxed);
                    let _ = fs::remove_file(REC_MARKER);

                    // Wait for recording thread
                    let handle = rec_handle.lock().unwrap().take();
                    if let Some(h) = handle {
                        let _ = h.join();
                    }

                    *state.lock().unwrap() = DaemonState::Transcribing;
                    let stored_tmux = tmux_env.lock().unwrap().clone();
                    tmux_message(&stored_tmux, "Transcriberen...");
                    writeln!(writer, "transcribing")?;
                    // Flush so client sees response before we block on transcription
                    writer.flush()?;

                    let wav_path = current_wav.lock().unwrap().take();
                    if let Some(wav_path) = wav_path {
                        // Transcribe
                        match rt.block_on(transcribe(&config.api_key, &wav_path, &config.lang)) {
                            Ok(text) => {
                                // Save transcript
                                let txt_path = wav_path.with_extension("txt");
                                if let Err(e) = fs::write(&txt_path, &text) {
                                    eprintln!("[voice-clip] Failed to save transcript: {}", e);
                                }

                                // Copy to clipboard
                                if let Err(e) = copy_to_clipboard(&text) {
                                    eprintln!("[voice-clip] Clipboard error: {}", e);
                                    let err_msg = format!("\u{2717} Clipboard fout: {}", e);
                                    tmux_message(&stored_tmux, &err_msg);
                                } else {
                                    let preview: String = text.chars().take(60).collect();
                                    let msg = format!("\u{2713} gekopieerd \u{2014} {}", preview);
                                    tmux_message(&stored_tmux, &msg);
                                }
                            }
                            Err(e) => {
                                eprintln!("[voice-clip] Transcription error: {}", e);
                                let err_msg = format!("\u{2717} Transcriptie fout: {}", e);
                                tmux_message(&stored_tmux, &err_msg);
                            }
                        }
                    }

                    *state.lock().unwrap() = DaemonState::Idle;
                }
                DaemonState::Transcribing => {
                    writeln!(writer, "busy (transcribing)")?;
                }
            }
        }
        "status" => {
            let current = *state.lock().unwrap();
            writeln!(writer, "{}", current)?;
        }
        "stop" => {
            writeln!(writer, "stopping")?;
            recording_flag.store(true, Ordering::Relaxed);
            running.store(false, Ordering::Relaxed);
        }
        other => {
            writeln!(writer, "unknown command: {}", other)?;
        }
    }

    Ok(())
}

fn ctrlc_setup(running: Arc<AtomicBool>, recording_flag: Arc<AtomicBool>) {
    // Use a simple signal handler via unsafe libc
    // We register for SIGTERM and SIGINT
    unsafe {
        let running_ptr = Arc::into_raw(running.clone());
        let recording_ptr = Arc::into_raw(recording_flag.clone());

        // Store in static so the handler can access them
        SIGNAL_RUNNING.store(running_ptr as *mut (), Ordering::SeqCst);
        SIGNAL_RECORDING.store(recording_ptr as *mut (), Ordering::SeqCst);

        libc::signal(libc::SIGINT, signal_handler as libc::sighandler_t);
        libc::signal(libc::SIGTERM, signal_handler as libc::sighandler_t);
    }
}

static SIGNAL_RUNNING: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static SIGNAL_RECORDING: std::sync::atomic::AtomicPtr<()> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

extern "C" fn signal_handler(_sig: libc::c_int) {
    let running_ptr = SIGNAL_RUNNING.load(Ordering::SeqCst);
    let recording_ptr = SIGNAL_RECORDING.load(Ordering::SeqCst);
    if !running_ptr.is_null() {
        let running = unsafe { &*(running_ptr as *const AtomicBool) };
        running.store(false, Ordering::Relaxed);
    }
    if !recording_ptr.is_null() {
        let recording = unsafe { &*(recording_ptr as *const AtomicBool) };
        recording.store(true, Ordering::Relaxed);
    }
}

fn cleanup() {
    fs::remove_file(SOCKET_PATH).ok();
    fs::remove_file(PID_PATH).ok();
    fs::remove_file(REC_MARKER).ok();
}

// ─── Client Commands ─────────────────────────────────────────────────────────

fn send_command(cmd: &str) -> Result<String> {
    let mut stream =
        UnixStream::connect(SOCKET_PATH).context("Cannot connect to daemon. Is it running?")?;

    stream
        .write_all(cmd.as_bytes())
        .context("Failed to send command")?;
    stream
        .write_all(b"\n")
        .context("Failed to send newline")?;
    stream.flush()?;

    // Shut down the write half so the daemon knows the command is complete
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    let mut reader = BufReader::new(&stream);
    reader.read_line(&mut response)?;

    Ok(response.trim().to_string())
}

fn cmd_toggle() -> Result<()> {
    let tmux = env::var("TMUX").unwrap_or_default();
    let cmd = if tmux.is_empty() {
        "toggle".to_string()
    } else {
        format!("toggle TMUX={}", tmux)
    };
    let resp = send_command(&cmd)?;
    println!("{}", resp);
    Ok(())
}

fn cmd_status() -> Result<()> {
    let resp = send_command("status")?;
    println!("{}", resp);
    Ok(())
}

fn cmd_stop() -> Result<()> {
    let resp = send_command("stop")?;
    println!("{}", resp);
    Ok(())
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = env::args().collect();
    let subcmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    let result = match subcmd {
        "daemon" => run_daemon(),
        "toggle" => cmd_toggle(),
        "status" => cmd_status(),
        "stop" => cmd_stop(),
        _ => {
            eprintln!("Usage: voice-clip <daemon|toggle|status|stop>");
            eprintln!();
            eprintln!("  daemon  — run background service");
            eprintln!("  toggle  — start/stop recording");
            eprintln!("  status  — print current state (idle/recording/transcribing)");
            eprintln!("  stop    — gracefully stop the daemon");
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("[voice-clip] Error: {:#}", e);
        std::process::exit(1);
    }
}
