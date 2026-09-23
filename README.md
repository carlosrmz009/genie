<div align="center">

<img src="icon.png" width="112" alt="ytplay icon: a gold eighth note on a walnut tile">

# ytplay

**Turn any piano video into a player-piano roll you can watch and hear.**

[![Rust](https://img.shields.io/badge/Rust-5A3A22?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Windows](https://img.shields.io/badge/Windows-5A3A22?style=flat-square&logo=windows&logoColor=white)](#requirements)
[![CUDA](https://img.shields.io/badge/CUDA-optional-C29545?style=flat-square&logo=nvidia&logoColor=white)](#gpu-transcription)
[![Transkun](https://img.shields.io/badge/transcription-Transkun-C29545?style=flat-square)](https://github.com/Yujia-Yan/Transkun)

<br>

<img src="docs/playing.png" width="860" alt="A piano roll scrolling down onto an 88-key keyboard in a walnut cabinet">

</div>

<br>

Paste a YouTube link, or just type the name of a piece. ytplay downloads the audio, transcribes every note with a neural network, and plays the result back through a piano SoundFont while the notes scroll down a paper roll onto the keys.

## Features

- **Link or search.** Paste a URL or type *"satie gymnopedie 1 piano"*; the first result is used.
- **GPU transcription.** [Transkun](https://github.com/Yujia-Yan/Transkun) runs on CUDA when it can and falls back to the CPU when it can't.
- **Cached.** Every download and transcription is kept, so a piece you've played before starts without being downloaded or transcribed again.
- **A real-looking instrument.** Walnut cabinet, brass tracker bar, punched paper roll, and a 2.5D keyboard whose keys go down as they play.
- **Playable.** Click or drag across the keys on the title screen to play them yourself.
- **Replay.** When the roll ends, press <kbd>Enter</kbd> to play it again or <kbd>Esc</kbd> to pick another piece.
- **One file.** A single ~4 MB `.exe` with every texture and the icon built in.

<div align="center">
<img src="docs/menu.png" width="640" alt="The title screen: a walnut panel with a paper search field and a brass Play button">
</div>

## Requirements

| | |
|---|---|
| **OS** | Windows 10 or 11 |
| **Tools on `PATH`** | [`yt-dlp`](https://github.com/yt-dlp/yt-dlp), [`ffmpeg`](https://ffmpeg.org), `python` |
| **Transcription** | `transkun` (Python package) |
| **Sound** | Any piano SoundFont (`.sf2`) |
| **To build** | Rust toolchain (MSVC) |

## Getting started

**1. Install the tools**

```bash
winget install yt-dlp.yt-dlp Gyan.FFmpeg
```

```bash
pip install transkun "setuptools<81"
```

Transkun still imports `pkg_resources`, which newer `setuptools` releases removed; the version pin keeps it working.

**2. Add a SoundFont**

Put a piano SoundFont at `%USERPROFILE%\ytplay\soundfont.sf2`. The free [Salamander Grand Piano](https://freepats.zenvoid.org/Piano/acoustic-grand-piano.html) is a good choice.

**3. Build and run**

```bash
cargo build --release
```

The app is `target\release\ytplay.exe`. Copy it anywhere you like.

### GPU transcription

With an NVIDIA GPU, install the CUDA build of PyTorch so Transkun runs on the GPU:

```bash
pip install --force-reinstall torch torchaudio --index-url https://download.pytorch.org/whl/cu130
```

Without it, ytplay transcribes on the CPU automatically.

## Controls

| Key | Where | Action |
|---|---|---|
| <kbd>Enter</kbd> | Title screen | Download, transcribe and play |
| Mouse | Title screen | Play the piano yourself |
| <kbd>Esc</kbd> | While playing | Back to the title screen |
| <kbd>Enter</kbd> | End of the roll | Play the piece again |

## How it works

```mermaid
flowchart LR
    A["YouTube link or search"] -- yt-dlp --> B["Audio (.mp3)"]
    B -- "Transkun (CUDA or CPU)" --> C["MIDI"]
    C -- "rustysynth + SoundFont" --> D["Speakers (WASAPI)"]
    C -- macroquad --> E["Piano roll"]
```

The audio thread is the clock: the roll is drawn from the number of samples sent to the speakers, so picture and sound can't drift apart. Everything ytplay keeps lives in `%USERPROFILE%\ytplay`:

```
ytplay\
├── soundfont.sf2       the piano sound
└── cache\
    ├── <video id>.mp3  downloaded audio
    └── <video id>.mid  transcription
```

To change the app icon, replace `icon.png` and rebuild; `build.rs` generates the window and `.exe` icons from it.

## Built with

- [Transkun](https://github.com/Yujia-Yan/Transkun): piano transcription
- [yt-dlp](https://github.com/yt-dlp/yt-dlp): audio download
- [rustysynth](https://github.com/sinshu/rustysynth): SoundFont synthesizer
- [macroquad](https://github.com/not-fl3/macroquad): window and drawing
- [cpal](https://github.com/RustAudio/cpal): audio output
- [midly](https://github.com/kovaxis/midly): MIDI parsing
- [Black Walnut Veneer 02](https://polyhaven.com/a/black_walnut_veneer_02) from Poly Haven (CC0): cabinet wood
