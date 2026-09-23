#![windows_subsystem = "windows"] // no console window; status/errors are shown in the GUI

use anyhow::{bail, Context, Result};
use macroquad::prelude::*;
use macroquad::text::Font;
use macroquad::ui::{hash, root_ui, widgets, Skin};
use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};
use std::{fs, path::PathBuf, process::Command, thread};
use std::sync::{atomic::{AtomicU64, Ordering}, mpsc, Arc, Mutex, OnceLock};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const LOOKAHEAD: f32 = 3.0; // seconds of notes visible above the keyboard
const SF2_NAME: &str = "soundfont.sf2";

struct Note { start: f32, end: f32, key: u8 }

/// %USERPROFILE%\ytplay holds the soundfont and the download/MIDI cache, so the exe can live anywhere.
/// Not AppData: packaged apps (Store Python running transkun, MSIX launchers) get AppData writes
/// silently redirected into their private package folder, where this exe can't see them.
fn data_dir() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join("ytplay")
}

fn run(cmd: &mut Command) -> Result<String> {
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(cmd, 0x0800_0000); // CREATE_NO_WINDOW
    let out = cmd.output().with_context(|| format!("failed to start {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Loaded once, shared by every song. Preloaded at startup; concurrent callers block until it's ready.
fn font() -> Result<Arc<SoundFont>> {
    static F: OnceLock<Result<Arc<SoundFont>, String>> = OnceLock::new();
    F.get_or_init(|| {
        let path = data_dir().join(SF2_NAME);
        let mut f = fs::File::open(&path).map_err(|e| format!("soundfont {}: {e}", path.display()))?;
        SoundFont::new(&mut f).map(Arc::new).map_err(|e| e.to_string())
    }).clone().map_err(anyhow::Error::msg)
}

/// URL / search text / .mid path -> (title, MIDI bytes). Audio (yt-dlp) and MIDI (transkun) are cached by video id.
fn transcribe(query: &str, status: &Mutex<String>) -> Result<(String, Vec<u8>)> {
    let set = |s: &str| *status.lock().unwrap() = s.to_string();
    if query.ends_with(".mid") {
        return Ok((query.to_string(), fs::read(query)?));
    }
    let target = if query.starts_with("http") { query.to_string() } else { format!("ytsearch1:{query}") };
    let cache = data_dir().join("cache");
    fs::create_dir_all(&cache)?;
    set("Downloading audio...");
    let out = run(Command::new("yt-dlp")
        .args(["-x", "--audio-format", "mp3", "--no-playlist", "--no-simulate", "--encoding", "utf-8", "--print", "after_move:title", "--print", "after_move:filepath", "-o"])
        .arg(cache.join("%(id)s.%(ext)s"))
        .arg(&target))?;
    let mut lines = out.lines().rev();
    let audio = PathBuf::from(lines.next().context("yt-dlp printed no file path")?);
    let title = lines.next().unwrap_or(query).to_string();
    let midi = audio.with_extension("mid");
    if !midi.exists() {
        // python -m: pip's Scripts dir often isn't on PATH. GPU path needs a CUDA build of torch.
        let transkun = |device: &str| run(Command::new("python").args(["-m", "transkun.transcribe", "--device", device]).arg(&audio).arg(&midi));
        set(&format!("Transcribing \"{title}\" on GPU..."));
        if transkun("cuda").is_err() {
            set(&format!("GPU failed, transcribing \"{title}\" on CPU (slow)..."));
            transkun("cpu")?;
        }
    }
    set("Loading soundfont...");
    font()?;
    Ok((title, fs::read(&midi)?))
}

const LEAD: f32 = 3.0; // blank roll leader, with the title printed on it, before the music starts
const GAIN: f32 = 2.5; // rustysynth peaks around 0.1-0.4 on transcriptions; bring it up to a normal level

/// Soft limiter: unity gain up to 0.8, then eases toward 1.0 instead of hard-clipping into crackle.
fn limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.8 { x } else { x.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh()) }
}

enum Screen {
    Menu,
    Busy(mpsc::Receiver<Result<(String, Vec<u8>)>>),
    Play { title: String, bytes: Vec<u8>, notes: Vec<Note>, length: f32, clock: Arc<AtomicU64>, rate: f32, _audio: cpal::Stream },
}

/// The default output device at its own rate and channel count (WASAPI), so Windows doesn't resample or convert.
fn output() -> Result<(cpal::Device, cpal::StreamConfig)> {
    let device = cpal::default_host().default_output_device().context("no audio output device")?;
    let config = device.default_output_config()?.config();
    Ok((device, config))
}

/// Starts a stream that asks `render` for stereo audio, then applies gain and the soft limiter.
fn stream(device: &cpal::Device, config: cpal::StreamConfig, mut render: impl FnMut(&mut [f32], &mut [f32]) + Send + 'static) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let (mut l, mut r) = (Vec::new(), Vec::new());
    let stream = device.build_output_stream(config, move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let n = data.len() / channels;
        l.resize(n, 0.0); // only allocates if the device asks for a bigger block than before
        r.resize(n, 0.0);
        render(&mut l, &mut r);
        for (i, frame) in data.chunks_mut(channels).enumerate() {
            frame.fill(0.0);
            frame[0] = limit(l[i] * GAIN);
            if channels > 1 { frame[1] = limit(r[i] * GAIN); }
        }
    }, |e| eprintln!("audio: {e}"), None)?;
    stream.play()?;
    Ok(stream)
}

fn start_play(title: String, bytes: Vec<u8>) -> Result<Screen> {
    let notes = load_notes(&bytes)?;
    let length = notes.iter().map(|n| n.end).fold(0.0, f32::max);
    let (device, config) = output()?;
    let rate = config.sample_rate;
    let mut settings = SynthesizerSettings::new(rate as i32);
    settings.maximum_polyphony = 256; // sustain-pedal passages hold many voices; the default 64 cuts notes off
    let mut seq = MidiFileSequencer::new(Synthesizer::new(&font()?, &settings)?);
    seq.play(&Arc::new(MidiFile::new(&mut &bytes[..])?), false);
    // The audio thread is the clock: frames handed to the device so far. Silence while the leader scrolls by.
    let clock = Arc::new(AtomicU64::new(0));
    let audio_clock = clock.clone();
    let lead = (LEAD * rate as f32) as u64;
    let audio = stream(&device, config, move |l, r| {
        if audio_clock.fetch_add(l.len() as u64, Ordering::Relaxed) < lead { l.fill(0.0); r.fill(0.0); return; }
        seq.render(l, r);
    })?;
    Ok(Screen::Play { title, bytes, notes, length, clock, rate: rate as f32, _audio: audio })
}

/// The menu's keyboard, played with the mouse: a synth the UI sends notes to, and the note being held.
struct LivePiano { synth: Arc<Mutex<Synthesizer>>, held: Option<u8>, _audio: cpal::Stream }

impl LivePiano {
    fn new() -> Result<Self> {
        let (device, config) = output()?;
        let synth = Arc::new(Mutex::new(Synthesizer::new(&font()?, &SynthesizerSettings::new(config.sample_rate as i32))?));
        let audio_synth = synth.clone();
        let audio = stream(&device, config, move |l, r| audio_synth.lock().unwrap().render(l, r))?;
        Ok(LivePiano { synth, held: None, _audio: audio })
    }

    /// Releases the held note (if any) and presses `key` (if any); dragging across keys plays each in turn.
    fn hold(&mut self, key: Option<u8>) {
        if key == self.held { return; }
        let mut synth = self.synth.lock().unwrap();
        if let Some(k) = self.held { synth.note_off(0, k as i32); }
        if let Some(k) = key { synth.note_on(0, k as i32, 96); }
        self.held = key;
    }
}

/// Flatten all tracks into absolute-time notes, honoring the tempo map.
fn load_notes(bytes: &[u8]) -> Result<Vec<Note>> {
    let smf = Smf::parse(bytes)?;
    let mut events: Vec<(u64, TrackEventKind)> = vec![];
    for track in &smf.tracks {
        let mut tick = 0u64;
        for e in track {
            tick += e.delta.as_int() as u64;
            events.push((tick, e.kind));
        }
    }
    events.sort_by_key(|e| e.0); // stable, keeps per-track order at equal ticks

    let (tpb, mut sec_per_tick) = match smf.header.timing {
        Timing::Metrical(t) => (t.as_int() as f64, 0.5 / t.as_int() as f64), // default 120 bpm
        Timing::Timecode(fps, sub) => (0.0, 1.0 / (fps.as_f32() as f64 * sub as f64)),
    };
    let (mut now, mut last) = (0.0f64, 0u64);
    let mut open = [[None::<f32>; 128]; 16];
    let mut notes = vec![];
    for (tick, kind) in events {
        now += (tick - last) as f64 * sec_per_tick;
        last = tick;
        match kind {
            TrackEventKind::Meta(MetaMessage::Tempo(us)) if tpb > 0.0 => sec_per_tick = us.as_int() as f64 / 1e6 / tpb,
            TrackEventKind::Midi { channel, message } => {
                let slot = |k: midly::num::u7| (channel.as_int() as usize, k.as_int());
                let (on, key) = match message {
                    MidiMessage::NoteOn { key, vel } if vel > 0 => (true, key),
                    MidiMessage::NoteOn { key, .. } | MidiMessage::NoteOff { key, .. } => (false, key),
                    _ => continue,
                };
                let (ch, k) = slot(key);
                if let Some(start) = open[ch][k as usize].take() {
                    notes.push(Note { start, end: now as f32, key: k });
                }
                if on { open[ch][k as usize] = Some(now as f32); }
            }
            _ => {}
        }
    }
    Ok(notes)
}

fn is_black(k: u8) -> bool { matches!(k % 12, 1 | 3 | 6 | 8 | 10) }

/// x position and width of a piano key (A0=21..C8=108) for a given white-key width.
fn key_rect(k: u8, ww: f32) -> (f32, f32) {
    let whites_before = (21..k).filter(|&n| !is_black(n)).count() as f32;
    if is_black(k) { (whites_before * ww - ww * 0.3, ww * 0.6) } else { (whites_before * ww, ww) }
}

// Player-piano cabinet: satin walnut case, manila roll behind a recessed window, brass hardware,
// felt, ivory and ebony. Every material is generated procedurally at startup, so the exe needs no image files.
const PAPER: Color = Color::from_hex(0xE3CF9E);
const PAPER_DARK: Color = Color::from_hex(0xCDB57F);
const PAPER_EDGE: Color = Color::from_hex(0xB89F68);
const INK: Color = Color::from_hex(0x4A3423);
const HOLE: Color = Color::from_hex(0x1C130D);
const BRASS: Color = Color::from_hex(0xC29545);
const BRASS_HI: Color = Color::from_hex(0xF5DC9C);
const BRASS_LO: Color = Color::from_hex(0x6E4F1E);
const GILT: Color = Color::from_hex(0xE2C07A);
const GILT_SOFT: Color = Color::from_hex(0xC9AB72);
const FELT_LIGHT: Color = Color::from_hex(0x9A2A2E);
const FELT_DARK: Color = Color::from_hex(0x58121A);
const KEY_LINE: Color = Color::from_hex(0x7E7870); // crisp grey outline between keys
const ERROR_INK: Color = Color::from_hex(0x8E2A1E);

const HEADER_H: f32 = 58.0;
const TRACKER_H: f32 = 14.0;
const FELT_H: f32 = 7.0;
const SLIP_H: f32 = 22.0;

fn alpha(c: Color, a: f32) -> Color { Color { a, ..c } }
fn mix(a: Color, b: Color, t: f32) -> Color {
    Color::new(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t, 1.0)
}

// ---- procedural materials ----

fn hash2(x: i32, y: i32) -> f32 {
    let h = (x as u32).wrapping_mul(374_761_393) ^ (y as u32).wrapping_mul(668_265_263);
    let h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) & 0xffff) as f32 / 65535.0
}

fn noise(x: f32, y: f32) -> f32 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (u, v) = (s(x - x.floor()), s(y - y.floor()));
    let (a, b, c, d) = (hash2(xi, yi), hash2(xi + 1, yi), hash2(xi, yi + 1), hash2(xi + 1, yi + 1));
    a + (b - a) * u + (c - a) * v + (a - b - c + d) * u * v
}

fn fbm(x: f32, y: f32) -> f32 {
    let (mut v, mut amp, mut f) = (0.0, 0.5, 1.0);
    for _ in 0..4 { v += amp * noise(x * f, y * f); amp *= 0.5; f *= 2.0; }
    v
}

fn gen_texture(w: u16, h: u16, f: impl Fn(f32, f32) -> Color) -> Texture2D {
    let mut bytes = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let c = f(x as f32 / w as f32, y as f32 / h as f32);
            bytes.extend([c.r, c.g, c.b, 1.0].map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8));
        }
    }
    Texture2D::from_rgba8(w, h, &bytes)
}

// Photographed veneers from Poly Haven (CC0, polyhaven.com), embedded so the exe stays self-contained.
// The scan is raw, unfinished wood; the tint below gives it an oiled finish when drawn.
const WALNUT_JPG: &[u8] = include_bytes!("../assets/black_walnut_veneer_02_diff_2k.jpg");
const WALNUT_TINT: Color = Color::new(0.62, 0.47, 0.36, 1.0); // oiled walnut

/// Decodes an embedded JPEG; `rotate` turns the grain from horizontal to vertical (for the cheek blocks).
fn photo(bytes: &[u8], rotate: bool) -> Texture2D {
    let img = Image::from_file_with_format(bytes, Some(image::ImageFormat::Jpeg)).expect("embedded texture is a valid JPEG");
    if !rotate { return Texture2D::from_image(&img); }
    let (w, h) = (img.width as usize, img.height as usize);
    let mut bytes = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (src, dst) = ((y * w + x) * 4, (x * h + y) * 4);
            bytes[dst..dst + 4].copy_from_slice(&img.bytes[src..src + 4]);
        }
    }
    Texture2D::from_image(&Image { bytes, width: img.height, height: img.width })
}

struct Assets {
    roman: Option<Font>,
    italic: Option<Font>,
    walnut: Texture2D,   // horizontal grain: case, nameboard, key slip
    walnut_v: Texture2D, // vertical grain: cheek blocks
    paper: Texture2D,
    felt: Texture2D,
}

fn make_assets() -> Assets {
    // macroquad's built-in font is ASCII-only; Palatino renders accented titles (e.g. "Gymnopédie").
    let load = |name: &str| fs::read(format!("C:/Windows/Fonts/{name}")).ok().and_then(|b| load_ttf_font_from_bytes(&b).ok());
    Assets {
        roman: load("pala.ttf").or_else(|| load("segoeui.ttf")),
        italic: load("palai.ttf").or_else(|| load("segoeui.ttf")),
        walnut: photo(WALNUT_JPG, false),
        walnut_v: photo(WALNUT_JPG, true),
        paper: gen_texture(512, 512, |u, v| {
            let mottle = fbm(u * 6.0, v * 6.0);
            let fibers = fbm(u * 90.0, v * 14.0);
            mix(PAPER, PAPER_DARK, (0.55 * mottle + 0.45 * fibers - 0.3).max(0.0) * 0.9)
        }),
        felt: gen_texture(512, 16, |u, v| mix(FELT_LIGHT, FELT_DARK, 0.3 + 0.7 * fbm(u * 260.0, v * 9.0))),
    }
}

// ---- drawing primitives ----

fn tex(t: &Texture2D, x: f32, y: f32, w: f32, h: f32, src: Rect, flip_y: bool, tint: Color) {
    draw_texture_ex(t, x, y, tint, DrawTextureParams { dest_size: Some(vec2(w, h)), source: Some(src), flip_y, ..Default::default() });
}

/// Rectangle shaded from `a` to `b`, top to bottom (or left to right when `horizontal`).
fn grad(x: f32, y: f32, w: f32, h: f32, a: Color, b: Color, horizontal: bool) {
    let (c1, c2, c3, c4) = if horizontal { (a, b, b, a) } else { (a, a, b, b) };
    let v = |x, y, c| Vertex::new(x, y, 0.0, 0.0, 0.0, c);
    draw_mesh(&Mesh {
        vertices: vec![v(x, y, c1), v(x + w, y, c2), v(x + w, y + h, c3), v(x, y + h, c4)],
        indices: vec![0, 1, 2, 0, 2, 3],
        texture: None,
    });
}

/// Walnut drawn 1:1 so the grain stays crisp; `seed_y` picks a different board from the texture.
fn walnut(a: &Assets, x: f32, y: f32, w: f32, h: f32, seed_y: f32) {
    let (sw, sh) = (w.min(2048.0), h.min(2048.0));
    let sy = seed_y.min(2048.0 - sh);
    tex(&a.walnut, x, y, w, h, Rect::new(x.min(2048.0 - sw), sy, sw, sh), false, WALNUT_TINT);
}

fn text(font: &Option<Font>, s: &str, x: f32, y: f32, size: u16, color: Color) {
    draw_text_ex(s, x, y, TextParams { font: font.as_ref(), font_size: size, color, ..Default::default() });
}

/// Gilt letters inlaid in wood: a dark cut below, the gold on top.
fn gilt(font: &Option<Font>, s: &str, x: f32, y: f32, size: u16, color: Color) {
    text(font, s, x + 1.0, y + 2.0, size, alpha(BLACK, 0.6));
    text(font, s, x, y, size, color);
}

fn text_width(font: &Option<Font>, s: &str, size: u16) -> f32 { measure_text(s, font.as_ref(), size, 1.0).width }

fn clock_str(t: f32) -> String { format!("{}:{:02}", (t.max(0.0) / 60.0) as u32, t.max(0.0) as u32 % 60) }

/// Polished brass plate with a bevelled edge.
fn brass(x: f32, y: f32, w: f32, h: f32, lit: bool) {
    let hi = if lit { WHITE } else { BRASS_HI };
    grad(x, y, w, h * 0.45, hi, BRASS, false);
    grad(x, y + h * 0.45, w, h * 0.55, BRASS, BRASS_LO, false);
    draw_rectangle_lines(x, y, w, h, 1.0, alpha(BRASS_LO, 0.9));
    draw_line(x + 1.0, y + 1.0, x + w - 1.0, y + 1.0, 1.0, alpha(WHITE, 0.6));
}

const KEY_R: f32 = 3.0; // corner radius at the front of every key

/// The last `r` px of a key, ending in two rounded corners at `bottom`.
fn round_bottom(x: f32, bottom: f32, w: f32, r: f32, c: Color) {
    draw_rectangle(x + r, bottom - r, w - 2.0 * r, r, c);
    for (cx, from) in [(x + r, 90.0f32), (x + w - r, 0.0)] {
        let cy = bottom - r;
        for i in 0..6 {
            let (a0, a1) = ((from + i as f32 * 15.0).to_radians(), (from + (i + 1) as f32 * 15.0).to_radians());
            draw_triangle(vec2(cx, cy), vec2(cx + r * a0.cos(), cy + r * a0.sin()), vec2(cx + r * a1.cos(), cy + r * a1.sin()), c);
        }
    }
}

/// A screwed-on brass plate with its label stamped in. Returns true when clicked.
fn brass_button(a: &Assets, label: &str, x: f32, y: f32, w: f32, h: f32) -> bool {
    let hover = Rect::new(x, y, w, h).contains(mouse_position().into());
    draw_rectangle(x + 2.0, y + 3.0, w, h, alpha(BLACK, 0.4));
    brass(x, y, w, h, hover);
    for (sx, sy, ang) in [(x + 8.0, y + 8.0, 0.4), (x + w - 8.0, y + 8.0, 1.9), (x + 8.0, y + h - 8.0, 2.8), (x + w - 8.0, y + h - 8.0, 0.9)] {
        screw(sx, sy, ang);
    }
    let lw = text_width(&a.roman, label, 24);
    text(&a.roman, label, x + (w - lw) / 2.0, y + h / 2.0 + 9.0, 24, alpha(WHITE, 0.45));
    text(&a.roman, label, x + (w - lw) / 2.0, y + h / 2.0 + 8.0, 24, Color::from_hex(0x3A2410));
    hover && is_mouse_button_pressed(MouseButton::Left)
}

fn screw(cx: f32, cy: f32, angle: f32) {
    draw_circle(cx, cy + 0.8, 3.6, alpha(BLACK, 0.45));
    draw_circle(cx, cy, 3.4, BRASS_LO);
    draw_circle(cx - 0.6, cy - 0.6, 2.6, BRASS_HI);
    let (dx, dy) = (angle.cos() * 3.0, angle.sin() * 3.0);
    draw_line(cx - dx, cy - dy, cx + dx, cy + dy, 1.2, BRASS_LO);
}

/// Routed panel moulding: shadow on the top/left edge, catch-light on the bottom/right.
fn panel(x: f32, y: f32, w: f32, h: f32) {
    draw_rectangle_lines(x, y, w, h, 2.0, alpha(BLACK, 0.45));
    draw_line(x + 2.0, y + h + 1.0, x + w + 1.0, y + h + 1.0, 1.0, alpha(WHITE, 0.14));
    draw_line(x + w + 1.0, y + 2.0, x + w + 1.0, y + h + 1.0, 1.0, alpha(WHITE, 0.14));
}

// ---- the instrument ----

/// Where everything sits: cheek blocks either side, keys between them, the roll window above.
struct Layout { w: f32, h: f32, x0: f32, ww: f32, bar_y: f32, felt_y: f32, kb_y: f32, kb_h: f32, slip_y: f32 }

fn layout(w: f32, h: f32) -> Layout {
    let x0 = (w * 0.02).clamp(16.0, 36.0);
    let ww = (w - 2.0 * x0) / 52.0;
    let kb_h = ww * 5.6;
    let slip_y = h - SLIP_H;
    let kb_y = slip_y - kb_h;
    let felt_y = kb_y - FELT_H;
    Layout { w, h, x0, ww, bar_y: felt_y - TRACKER_H, felt_y, kb_y, kb_h, slip_y }
}

/// Walnut case: body, cheek blocks, and the key slip rail in front of the keys.
fn draw_case(a: &Assets, l: &Layout) {
    walnut(a, 0.0, 0.0, l.w, l.h, 0.0);
    grad(0.0, 0.0, l.w, l.h * 0.5, alpha(WHITE, 0.06), alpha(WHITE, 0.0), false); // satin sheen from above
    for (x, shade_left) in [(0.0, false), (l.w - l.x0, true)] {
        tex(&a.walnut_v, x, 0.0, l.x0, l.h, Rect::new(if shade_left { 900.0 } else { 300.0 }, 0.0, l.x0, l.h.min(2048.0)), false, WALNUT_TINT);
        let (lo, hi) = (alpha(BLACK, 0.35), alpha(WHITE, 0.08));
        grad(x, 0.0, l.x0, l.h, if shade_left { lo } else { hi }, if shade_left { hi } else { lo }, true);
    }
    // Key slip: a rounded rail, lit on top.
    tex(&a.walnut, l.x0, l.slip_y, l.w - 2.0 * l.x0, SLIP_H, Rect::new(40.0, 700.0, (l.w - 2.0 * l.x0).min(2000.0), SLIP_H), false, WALNUT_TINT);
    grad(l.x0, l.slip_y, l.w - 2.0 * l.x0, SLIP_H * 0.4, alpha(WHITE, 0.22), alpha(WHITE, 0.0), false);
    grad(l.x0, l.slip_y + SLIP_H * 0.5, l.w - 2.0 * l.x0, SLIP_H * 0.5, alpha(BLACK, 0.0), alpha(BLACK, 0.35), false);
    draw_rectangle(l.x0, l.slip_y - 1.0, l.w - 2.0 * l.x0, 2.0, alpha(BLACK, 0.7));
}

const BLACK_LEN: f32 = 0.63; // black keys' length as a fraction of the white keys'

/// The key under screen point `p`, if any.
fn key_at(l: &Layout, p: Vec2) -> Option<u8> {
    let hit = |k: u8, len: f32| {
        let (x, kw) = key_rect(k, l.ww);
        Rect::new(l.x0 + x, l.kb_y, kw, l.kb_h * len).contains(p)
    };
    (21u8..=108).filter(|&k| is_black(k)).find(|&k| hit(k, BLACK_LEN))
        .or_else(|| (21u8..=108).filter(|&k| !is_black(k)).find(|&k| hit(k, 1.0)))
}

/// Tracker bar, felt strip and keyboard.
fn draw_keyboard(a: &Assets, l: &Layout, down: &[bool; 128]) {
    let kx = |k: u8| { let (x, kw) = key_rect(k, l.ww); (l.x0 + x, kw) };
    let span = l.w - 2.0 * l.x0;

    // Brass tracker bar, round in section: one port per key, glowing while air flows through a slot.
    grad(l.x0, l.bar_y, span, TRACKER_H * 0.35, BRASS_LO, BRASS_HI, false);
    grad(l.x0, l.bar_y + TRACKER_H * 0.35, span, TRACKER_H * 0.65, BRASS_HI, BRASS_LO, false);
    for k in 21u8..=108 {
        let (x, kw) = kx(k);
        let (cx, cy) = (x + kw / 2.0, l.bar_y + TRACKER_H * 0.45);
        if down[k as usize] {
            draw_circle(cx, cy, 7.0, alpha(BRASS_HI, 0.3));
            draw_circle(cx, cy, 2.8, WHITE);
        } else {
            draw_circle(cx, cy + 0.5, 2.2, alpha(WHITE, 0.35));
            draw_circle(cx, cy, 2.0, HOLE);
        }
    }

    // Felt strip, then the dark key bed that shows between keys.
    tex(&a.felt, l.x0, l.felt_y, span, FELT_H, Rect::new(0.0, 0.0, 512.0, 16.0), false, WHITE);
    draw_rectangle(l.x0, l.kb_y, span, l.kb_h, KEY_LINE); // shows as a 1px outline around every key

    // White keys, 2.5D: a gently curved face, bevelled long edges, and a front lip that shortens when pressed.
    let lip_h = (l.ww * 0.3).clamp(6.0, 11.0);
    for k in (21u8..=108).filter(|&k| !is_black(k)) {
        let (x, kw) = kx(k);
        let (x, kw) = (x + 0.5, kw - 1.0);
        let pressed = down[k as usize];
        let lip = if pressed { lip_h * 0.45 } else { lip_h };
        let face_h = l.kb_h - lip;
        let (top, bottom) = if pressed { (Color::from_hex(0xDCCBA6), Color::from_hex(0xF0E2C2)) } else { (Color::from_hex(0xE9E5DB), Color::from_hex(0xFFFEFA)) };
        grad(x, l.kb_y, kw, face_h, top, bottom, false);
        draw_rectangle(x, l.kb_y, 1.0, face_h, alpha(WHITE, 0.9));                 // lit left edge
        draw_rectangle(x + kw - 1.0, l.kb_y, 1.0, face_h, alpha(KEY_LINE, 0.35));  // shaded right edge
        // Front lip: bright rounded edge on top, then the face turning away from the light.
        let ly = l.kb_y + face_h;
        let (r, lip_end) = (KEY_R.min(lip), mix(bottom, KEY_LINE, 0.5));
        grad(x, ly, kw, lip - r, mix(bottom, KEY_LINE, 0.12), lip_end, false);
        round_bottom(x, l.kb_y + l.kb_h, kw, r, lip_end);
        draw_rectangle(x, ly, kw, 1.0, WHITE);
    }
    // The keys disappear under the felt rail; a pressed key sinks a little deeper into that shade.
    grad(l.x0, l.kb_y, span, 6.0, alpha(BLACK, 0.22), alpha(BLACK, 0.0), false);
    for k in (21u8..=108).filter(|&k| !is_black(k) && down[k as usize]) {
        let (x, kw) = kx(k);
        grad(x + 0.5, l.kb_y, kw - 1.0, 14.0, alpha(BLACK, 0.18), alpha(BLACK, 0.0), false);
    }

    // Black keys, 2.5D: a raised block with side bevels, a glossy top, a lighter sloped front,
    // and one small crisp shadow on the ivory below and to the right.
    let bh = l.kb_h * BLACK_LEN;
    for k in (21u8..=108).filter(|&k| is_black(k)) {
        let (x, kw) = kx(k);
        let pressed = down[k as usize];
        let front = bh * if pressed { 0.05 } else { 0.1 };
        let lift = if pressed { 1.5 } else { 3.0 };
        // Shadow along the right side and under the front; they stop short of the rounded corner so they don't overlap there.
        grad(x + kw, l.kb_y, lift, bh - KEY_R, alpha(BLACK, 0.28), alpha(BLACK, 0.0), true);
        grad(x + lift + KEY_R, l.kb_y + bh, kw - KEY_R, lift + 1.0, alpha(BLACK, 0.28), alpha(BLACK, 0.0), false);

        let (top, bottom, front_hi, front_lo) = if pressed {
            (Color::from_hex(0x6A5226), Color::from_hex(0x3A2C12), Color::from_hex(0x8C6E36), Color::from_hex(0x4A3818))
        } else {
            (Color::from_hex(0x2C2A29), Color::from_hex(0x121111), Color::from_hex(0x4E4B49), Color::from_hex(0x1C1B1A))
        };
        let r = KEY_R.min(front);
        draw_rectangle(x - 0.5, l.kb_y, kw + 1.0, bh - r, BLACK); // crisp outline, rounded like the key
        round_bottom(x - 0.5, l.kb_y + bh + 0.5, kw + 1.0, r + 0.5, BLACK);
        let bevel = (kw * 0.12).clamp(1.5, 3.0);
        let top_h = bh - front;
        draw_rectangle(x, l.kb_y, bevel, top_h, Color::from_hex(0x3A3836));              // lit left bevel
        draw_rectangle(x + kw - bevel, l.kb_y, bevel, top_h, Color::from_hex(0x050505)); // dark right bevel
        grad(x + bevel, l.kb_y, kw - 2.0 * bevel, top_h, top, bottom, false);            // glossy top
        grad(x, l.kb_y + top_h, kw, front - r, front_hi, front_lo, false);               // sloped front
        round_bottom(x, l.kb_y + bh, kw, r, front_lo);
        draw_rectangle(x + bevel, l.kb_y + top_h, kw - 2.0 * bevel, 1.0, alpha(WHITE, 0.25));
        // One soft highlight down the top, like light on polished lacquer.
        let (hx, hw) = (x + kw * 0.3, kw * 0.4);
        grad(hx, l.kb_y + 2.0, hw, top_h * 0.8, alpha(WHITE, 0.18), alpha(WHITE, 0.0), false);
    }
}

/// The roll: paper scrolling down onto the tracker bar, notes as punched slots, title printed on the leader.
fn draw_roll(a: &Assets, l: &Layout, title: &str, notes: &[Note], t: f32) {
    let mut down = [false; 128];
    for n in notes.iter().filter(|n| n.start <= t && n.end > t) { down[n.key as usize] = true; }

    draw_case(a, l);
    let (wx, wy, ww_, wh) = (l.x0, HEADER_H, l.w - 2.0 * l.x0, l.bar_y - HEADER_H);
    let px_per_sec = wh / LOOKAHEAD;
    let y_of = |time: f32| l.bar_y - (time - t) * px_per_sec;

    // Paper, tiled with alternate tiles mirrored so the scrolling texture has no seams.
    let tile = 640.0;
    let scroll = (t * px_per_sec).rem_euclid(tile * 2.0);
    let mut row: i32 = -2;
    while wy + row as f32 * tile + scroll < l.bar_y {
        let y = wy + row as f32 * tile + scroll;
        let mut x = wx;
        while x < wx + ww_ {
            let (y0, y1) = (y.max(wy), (y + tile).min(l.bar_y));
            let (dw, sy0, sy1) = ((wx + ww_ - x).min(tile), (y0 - y) / tile * 512.0, (y1 - y) / tile * 512.0);
            if y1 > y0 {
                let flip = row.rem_euclid(2) == 1;
                let src = if flip { Rect::new(0.0, 512.0 - sy1, dw / tile * 512.0, sy1 - sy0) } else { Rect::new(0.0, sy0, dw / tile * 512.0, sy1 - sy0) };
                tex(&a.paper, x, y0, dw, y1 - y0, src, flip, WHITE);
            }
            x += tile;
        }
        row += 1;
    }

    // Printed guides: a faint rule at every C and every second, so the eye can track pitch and pace.
    for k in (24u8..=108).step_by(12) {
        let x = l.x0 + key_rect(k, l.ww).0;
        draw_line(x, wy, x, l.bar_y, 1.0, alpha(INK, 0.12));
    }
    for s in (t.floor() as i32)..=((t + LOOKAHEAD).ceil() as i32) {
        let y = y_of(s as f32);
        if y > wy && y < l.bar_y { draw_line(wx, y, wx + ww_, y, 1.0, alpha(INK, 0.07)); }
    }

    // Leader: the title printed across the paper, then a ruled start line where the music begins.
    let ty = y_of(-LEAD / 2.0);
    if ty > wy && ty < l.bar_y + 60.0 {
        text(&a.italic, title, (l.w - text_width(&a.italic, title, 40)) / 2.0, ty, 40, INK);
        let sub = "transcribed by ytplay";
        text(&a.roman, sub, (l.w - text_width(&a.roman, sub, 20)) / 2.0, ty + 34.0, 20, alpha(INK, 0.7));
    }
    let sy = y_of(0.0);
    if sy > wy && sy < l.bar_y { draw_line(l.w * 0.2, sy, l.w * 0.8, sy, 1.5, alpha(INK, 0.5)); }

    for n in notes.iter().filter(|n| n.end > t && n.start < t + LOOKAHEAD) {
        let (x, kw) = key_rect(n.key, l.ww);
        let slot_w = if is_black(n.key) { kw * 0.62 } else { kw * 0.5 };
        draw_slot(l.x0 + x + kw / 2.0, slot_w, y_of(n.end).max(wy - 20.0), y_of(n.start).min(l.bar_y + slot_w));
    }

    // Recess: the case casts shadow onto the paper along the top and sides of the window.
    grad(wx, wy, ww_, 18.0, alpha(BLACK, 0.5), alpha(BLACK, 0.0), false);
    grad(wx, wy, 12.0, wh, alpha(BLACK, 0.35), alpha(BLACK, 0.0), true);
    grad(wx + ww_ - 12.0, wy, 12.0, wh, alpha(BLACK, 0.0), alpha(BLACK, 0.35), true);

    draw_keyboard(a, l, &down);

    // Nameboard: a walnut rail with a brass inlay line along its lower edge.
    walnut(a, 0.0, 0.0, l.w, HEADER_H, 300.0);
    grad(0.0, 0.0, l.w, HEADER_H * 0.5, alpha(WHITE, 0.08), alpha(WHITE, 0.0), false);
    draw_rectangle(0.0, HEADER_H - 3.0, l.w, 2.0, BRASS);
    draw_rectangle(0.0, HEADER_H - 1.0, l.w, 1.0, alpha(BLACK, 0.6));
}

/// A punched slot: a pill shape with a slightly darker cut edge in the paper around it.
fn draw_slot(cx: f32, w: f32, y0: f32, y1: f32) {
    for (pad, c) in [(1.6, PAPER_EDGE), (0.0, HOLE)] {
        let r = w / 2.0 + pad;
        let (a, b) = (y0 - pad + r, y1 + pad - r);
        if b > a { draw_rectangle(cx - r, a, r * 2.0, b - a, c); }
        draw_circle(cx, a.min(b), r, c);
        draw_circle(cx, b.max(a), r, c);
    }
}

/// Idle instrument for the menu and loading screens: case, a routed panel, keys at rest.
fn draw_idle(a: &Assets, l: &Layout, down: &[bool; 128]) {
    draw_case(a, l);
    panel(l.x0 + 24.0, 34.0, l.w - 2.0 * l.x0 - 48.0, l.bar_y - 64.0);
    draw_keyboard(a, l, down);
}

async fn ui_loop() {
    let assets = make_assets();
    let skin = {
        let ui = root_ui();
        let clear = alpha(BLACK, 0.0); // the paper slip behind the field is drawn by hand
        let mut b = ui.style_builder().font_size(26).text_color(INK).color(clear).color_hovered(clear)
            .color_clicked(clear).color_selected(clear).color_selected_hovered(clear).color_inactive(clear);
        if let Some(f) = &assets.roman { b = b.with_font(f).unwrap(); }
        Skin { editbox_style: b.build(), ..ui.default_skin() }
    };
    root_ui().push_skin(&skin);

    let status = Arc::new(Mutex::new(String::new()));
    let (mut query, mut error, mut screen) = (String::new(), String::new(), Screen::Menu);
    let mut live: Option<LivePiano> = None; // opened on the first click on a key, closed when leaving the menu
    loop {
        let (w, h) = (screen_width(), screen_height());
        let l = layout(w, h);
        let a = &assets;
        let mut next = None;
        let mut fail = |e: anyhow::Error| { error = format!("{e:#}"); Some(Screen::Menu) };
        match &screen {
            Screen::Menu => {
                let key = if is_mouse_button_down(MouseButton::Left) { key_at(&l, mouse_position().into()) } else { None };
                if key.is_some() && live.is_none() {
                    match LivePiano::new() { Ok(p) => live = Some(p), Err(e) => error = format!("{e:#}") }
                }
                if let Some(p) = &mut live { p.hold(key); }
                let mut down = [false; 128];
                if let Some(k) = key { down[k as usize] = true; }
                draw_idle(a, &l, &down);
                let cy = l.bar_y * 0.40;
                let mark = "ytplay";
                gilt(&a.italic, mark, (w - text_width(&a.italic, mark, 104)) / 2.0, cy, 104, GILT);
                let tag = "Turn a piano recording into a roll you can watch and hear.";
                gilt(&a.roman, tag, (w - text_width(&a.roman, tag, 22)) / 2.0, cy + 66.0, 22, GILT_SOFT);

                // Paper slip in a brass frame for the query, a screwed-on brass plate for the action.
                let (bw, fh, gap, rim) = (124.0, 50.0, 14.0, 6.0);
                let fw = 620.0f32.min(w - bw - gap - 2.0 * l.x0 - 80.0);
                let fx = (w - fw - gap - bw) / 2.0;
                let fy = cy + 104.0;
                draw_rectangle(fx - rim + 2.0, fy - rim + 3.0, fw + rim * 2.0, fh + rim * 2.0, alpha(BLACK, 0.4));
                brass(fx - rim, fy - rim, fw + rim * 2.0, fh + rim * 2.0, false);
                tex(&a.paper, fx, fy, fw, fh, Rect::new(0.0, 100.0, 500.0, 40.0), false, WHITE);
                grad(fx, fy, fw, 8.0, alpha(BLACK, 0.3), alpha(BLACK, 0.0), false);
                if query.is_empty() {
                    text(&a.roman, "Paste a YouTube link or search for a piece", fx + 16.0, fy + 33.0, 22, alpha(INK, 0.55));
                }
                let id = hash!();
                root_ui().set_input_focus(id);
                widgets::InputText::new(id).position(vec2(fx + 8.0, fy + 6.0)).size(vec2(fw - 16.0, fh - 12.0)).ui(&mut root_ui(), &mut query);

                let clicked = brass_button(a, "Play", fx + fw + gap, fy - rim, bw, fh + rim * 2.0);
                let go = clicked || is_key_pressed(KeyCode::Enter);
                let q = query.trim().to_string();
                if go && !q.is_empty() {
                    error.clear();
                    *status.lock().unwrap() = "Starting".into();
                    let (tx, rx) = mpsc::channel();
                    let st = status.clone();
                    thread::spawn(move || tx.send(transcribe(&q, &st)));
                    next = Some(Screen::Busy(rx));
                }
                // Errors arrive on a paper note tucked under the field.
                let max_lines = ((l.bar_y - fy - fh - 70.0) / 22.0).max(0.0) as usize;
                let lines: Vec<&str> = error.lines().take(max_lines).collect();
                if !lines.is_empty() {
                    let (nx, ny, nw) = (fx, fy + fh + rim + 24.0, fw + gap + bw);
                    let nh = lines.len() as f32 * 22.0 + 20.0;
                    draw_rectangle(nx + 3.0, ny + 4.0, nw, nh, alpha(BLACK, 0.35));
                    tex(&a.paper, nx, ny, nw, nh, Rect::new(0.0, 0.0, 512.0, (nh / nw * 512.0).min(512.0)), false, WHITE);
                    for (i, line) in lines.iter().enumerate() {
                        text(&a.roman, line, nx + 14.0, ny + 26.0 + i as f32 * 22.0, 18, if i == 0 { ERROR_INK } else { INK });
                    }
                }
            }
            Screen::Busy(rx) => {
                draw_idle(a, &l, &[false; 128]);
                let s = status.lock().unwrap().trim_end_matches('.').to_string();
                let sw = text_width(&a.italic, &s, 34);
                let cy = l.bar_y * 0.5;
                gilt(&a.italic, &s, (w - sw) / 2.0, cy, 34, GILT);
                // One slow brass sweep under the status: the only idle motion in the app.
                let phase = (get_time() as f32 * 0.6).fract();
                let lw = sw.max(200.0);
                let lx = (w - lw) / 2.0;
                draw_rectangle(lx, cy + 20.0, lw, 1.0, alpha(BLACK, 0.4));
                draw_rectangle(lx + phase * (lw - 60.0), cy + 19.0, 60.0, 3.0, BRASS);
                next = match rx.try_recv() {
                    Ok(Ok((title, bytes))) => start_play(title, bytes).map_or_else(&mut fail, Some),
                    Ok(Err(e)) => fail(e),
                    Err(mpsc::TryRecvError::Disconnected) => fail(anyhow::anyhow!("worker thread crashed")),
                    Err(mpsc::TryRecvError::Empty) => None,
                };
            }
            Screen::Play { title, bytes, notes, length, clock, rate, .. } => {
                let t = clock.load(Ordering::Relaxed) as f32 / rate - LEAD;
                draw_roll(a, &l, title, notes, t);
                gilt(&a.italic, title, l.x0 + 6.0, 38.0, 26, GILT);
                let time = format!("{} / {}", clock_str(t), clock_str(*length));
                let tw = text_width(&a.roman, &time, 22);
                gilt(&a.roman, &time, w - l.x0 - tw - 6.0, 37.0, 22, GILT);
                let hint = "Esc returns to search";
                gilt(&a.roman, hint, w - l.x0 - tw - 34.0 - text_width(&a.roman, hint, 17), 36.0, 17, GILT_SOFT);
                if t > *length + 1.5 { // last note released and its tail has rung out
                    let (pw, ph) = (480.0f32.min(w - 2.0 * l.x0 - 40.0), 200.0);
                    let (px, py) = ((w - pw) / 2.0, HEADER_H + (l.bar_y - HEADER_H - ph) / 2.0);
                    draw_rectangle(l.x0, HEADER_H, w - 2.0 * l.x0, l.bar_y - HEADER_H, alpha(BLACK, 0.3));
                    draw_rectangle(px + 4.0, py + 6.0, pw, ph, alpha(BLACK, 0.45));
                    walnut(a, px, py, pw, ph, 900.0);
                    draw_rectangle_lines(px, py, pw, ph, 3.0, BRASS);
                    panel(px + 12.0, py + 12.0, pw - 24.0, ph - 24.0);
                    let head = "End of the roll";
                    gilt(&a.italic, head, (w - text_width(&a.italic, head, 36)) / 2.0, py + 64.0, 36, GILT);
                    let (bw, bh, gap) = ((pw - 72.0) / 2.0, 50.0, 16.0);
                    let by = py + ph - bh - 34.0;
                    if brass_button(a, "Play again", px + 28.0, by, bw, bh) || is_key_pressed(KeyCode::Enter) {
                        next = start_play(title.clone(), bytes.clone()).map_or_else(&mut fail, Some);
                    }
                    if brass_button(a, "Back to search", px + 28.0 + bw + gap, by, bw, bh) { next = Some(Screen::Menu); }
                }
                if is_key_pressed(KeyCode::Escape) { next = Some(Screen::Menu); } // dropping the stream stops audio
            }
        }
        if let Some(s) = next { screen = s; live = None; } // leaving the menu closes the playable keyboard's audio
        next_frame().await;
    }
}

fn main() {
    thread::spawn(|| { let _ = font(); }); // preload the soundfont while the user types
    // Window/taskbar icon, generated from icon.png by build.rs.
    let icon = macroquad::miniquad::conf::Icon {
        small: *include_bytes!(concat!(env!("OUT_DIR"), "/icon16.rgba")),
        medium: *include_bytes!(concat!(env!("OUT_DIR"), "/icon32.rgba")),
        big: *include_bytes!(concat!(env!("OUT_DIR"), "/icon64.rgba")),
    };
    let conf = Conf { window_title: "ytplay".into(), window_width: 1560, window_height: 800, icon: Some(icon), ..Default::default() };
    macroquad::Window::from_config(conf, ui_loop());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_map_and_keys() {
        // 1 track, 480 tpb: tempo 1s/beat, note 60 on at tick 0, off at tick 480; then note 62 480..960
        let bytes: Vec<u8> = [
            &b"MThd"[..], &[0, 0, 0, 6, 0, 0, 0, 1, 0x01, 0xE0],
            b"MTrk", &[0, 0, 0, 29],
            &[0x00, 0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40], // tempo 1_000_000 us
            &[0x00, 0x90, 60, 100],
            &[0x83, 0x60, 0x90, 60, 0], // delta 480, note-on vel 0 = off
            &[0x00, 0x90, 62, 100],
            &[0x83, 0x60, 0x80, 62, 0],
            &[0x00, 0xFF, 0x2F, 0x00],
        ].concat();
        let n = load_notes(&bytes).unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!((n[0].key, n[0].start, n[0].end), (60, 0.0, 1.0));
        assert_eq!((n[1].key, n[1].start, n[1].end), (62, 1.0, 2.0));
        assert_eq!(key_rect(21, 10.0).0, 0.0);
        assert_eq!(key_rect(108, 10.0).0, 510.0); // C8 is the 52nd white key

        // Clicking the menu keyboard: black keys sit on top of the white ones.
        let l = layout(1560.0, 800.0);
        let at = |k: u8, dx: f32, y: f32| key_at(&l, vec2(l.x0 + key_rect(k, l.ww).0 + dx, y));
        assert_eq!(at(61, 2.0, l.kb_y + 5.0), Some(61));            // C#4 near the back
        assert_eq!(at(60, l.ww - 2.0, l.kb_y + 5.0), Some(61));     // right edge of C4 at the back is under C#4
        assert_eq!(at(60, l.ww - 2.0, l.kb_y + l.kb_h - 3.0), Some(60)); // same x at the front is C4
        assert_eq!(at(60, 2.0, l.bar_y - 5.0), None);               // above the keyboard
    }
}
