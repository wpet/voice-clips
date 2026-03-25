# voice-clip

WSL2 voice-to-text Rust daemon. Toggle recording via `Alt+V`, captures audio (ALSA), transcribes via OpenAI Whisper API, copies to Windows clipboard. Recordings archived as WAV+TXT with timestamps. Tmux status bar shows ● REC indicator. Runs as systemd user service.

## Prerequisites

- WSL2 with USB microphone passthrough (tested with Logitech BRIO)
- `libasound2-dev` (ALSA development headers)
- Rust toolchain
- OpenAI API key
- tmux (optional, for status bar indicator and Alt+V keybinding)

## Installation

```bash
# Install ALSA dev headers
sudo apt install libasound2-dev

# Clone and build
git clone https://github.com/wpet/voice-clips.git
cd voice-clips/voice-clip
cargo build --release

# Symlink binary into PATH
ln -sf "$(pwd)/target/release/voice-clip" ~/.local/bin/voice-clip

# Create clips directory
mkdir -p ~/voice-clips/clips

# Configure API key
cat > ~/voice-clips/.env << 'EOF'
OPENAI_API_KEY=sk-your-key-here
EOF
```

## Configuration

Edit `~/voice-clips/.env`:

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENAI_API_KEY` | *(required)* | OpenAI API key for Whisper |
| `VOICE_CLIP_LANG` | `nl` | Transcription language |
| `VOICE_CLIP_DEVICE` | `plughw:0,0` | ALSA capture device |

## Usage

### Commands

```bash
voice-clip daemon   # Start background service
voice-clip toggle   # Start/stop recording
voice-clip status   # Show current state (idle/recording/transcribing)
voice-clip stop     # Gracefully stop daemon
```

### Systemd service (auto-start)

```bash
mkdir -p ~/.config/systemd/user

cat > ~/.config/systemd/user/voice-clip.service << 'EOF'
[Unit]
Description=Voice-clip daemon
After=default.target

[Service]
Type=simple
ExecStart=%h/.local/bin/voice-clip daemon
ExecStop=%h/.local/bin/voice-clip stop
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now voice-clip.service
```

### Tmux keybinding (Alt+V)

Add to `~/.tmux.conf`:

```tmux
# Voice-clip: Alt+V toggle recording
bind -n M-v run-shell -b "~/.local/bin/voice-clip toggle >/dev/null 2>&1"

# Status bar REC indicator (poll every 1s)
set -g status-interval 1
# Prepend to your existing status-right:
#   #([ -f /tmp/voice-clip.recording ] && echo "#[fg=#f38ba8,bold]● REC #[default]| ")
```

Then reload: `tmux source-file ~/.tmux.conf`

### Bash keybinding (Alt+V, outside tmux)

Add to `~/.bashrc`:

```bash
bind -x '"\ev":"voice-clip toggle"'
```

### Workflow

1. **`Alt+V`** — start recording (● REC appears in tmux status bar)
2. **`Alt+V`** — stop recording, transcription runs automatically
3. **`Ctrl+Shift+V`** — paste the transcribed text

### Archive

All recordings are saved with timestamps:

```
~/voice-clips/clips/
├── 2026-03-25_143052.wav
├── 2026-03-25_143052.txt
├── 2026-03-25_144510.wav
└── 2026-03-25_144510.txt
```

## Architecture

```
┌──────────────┐     Unix socket      ┌──────────────────┐
│  Alt+V       │ ──── toggle ───────► │  voice-clip      │
│  (tmux/bash) │ ◄─── status ──────── │  daemon          │
└──────────────┘                      │                  │
                                      │  ┌─ ALSA ──► WAV│
                                      │  ├─ Whisper ► TXT│
                                      │  └─ clip.exe ► 📋│
                                      └──────────────────┘
```

- **Daemon** listens on `/tmp/voice-clip.sock`
- **Client** (`voice-clip toggle`) sends commands via socket
- **Feedback** via `tmux display-message` (non-intrusive)
- **REC indicator** via marker file `/tmp/voice-clip.recording`

## License

MIT
