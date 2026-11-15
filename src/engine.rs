//! Realtime engine: lock-free cpal backend, fundsp synth voices (polyphonic pools),
//! PCM kit + pitched sampler playback routed through per-channel DSP,
//! delay/reverb sends, sample-clock scheduler, and disk-streaming take recorder.

use crate::song::{ChannelKind, KitPiece, Song, TICKS_PATTERN, VoiceSpec, WaveKind, eval_auto};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fundsp::prelude::{
    AudioUnit, Shared, Wave, adsr_live, dc, highpass_hz, lowpass, midi_hz, pan, reverb_stereo, saw,
    shared, sine, square, triangle, var, white,
};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const VOICES_PER_CH: usize = 8;
const MAX_PLAYHEADS: usize = 48;
const SCOPE_LEN: usize = 512;
const MAX_TELEM_CH: usize = 64;
const PENDING_CAP: usize = 1024;
const TAKE_RING_FLOATS: usize = 131_072;

struct VoiceRt {
    unit: Box<dyn AudioUnit + Send>,
    pitch: Shared,
    gate: Shared,
    cutoff: Shared,
    vel: f32,
    off: usize,
    age: u64,
}

const LIVE_VOICE_SPEC: VoiceSpec = VoiceSpec {
    wave: WaveKind::Saw,
    q: 1.0,
    attack: 0.005,
    decay: 0.08,
    sustain: 0.7,
    release: 0.1,
};

fn make_voice(
    spec: VoiceSpec,
    pitch: &Shared,
    gate: &Shared,
    cutoff: &Shared,
    sample_rate: f64,
) -> Box<dyn AudioUnit + Send> {
    let VoiceSpec {
        wave,
        q,
        attack: a,
        decay: d,
        sustain: s,
        release: r,
    } = spec;
    // Degenerate Q (<= 0) NaNs the SVF: clamp hard.
    let q = q.clamp(0.1, 8.0);
    // Voice = (pitch-driven osc through audio-rate lowpass) * live ADSR, stereo.
    // Cutoff rides a Shared var: automation moves it with no graph rebuilds.
    let unit: Box<dyn AudioUnit + Send> = match wave {
        WaveKind::Sine => Box::new(
            ((((var(pitch) >> sine::<f32>()) | var(cutoff) | dc(q)) >> lowpass::<f32>())
                * (var(gate) >> adsr_live(a, d, s, r)))
                >> pan(0.0),
        ),
        WaveKind::Triangle => Box::new(
            ((((var(pitch) >> triangle()) | var(cutoff) | dc(q)) >> lowpass::<f32>())
                * (var(gate) >> adsr_live(a, d, s, r)))
                >> pan(0.0),
        ),
        WaveKind::Saw => Box::new(
            ((((var(pitch) >> saw()) | var(cutoff) | dc(q)) >> lowpass::<f32>())
                * (var(gate) >> adsr_live(a, d, s, r)))
                >> pan(0.0),
        ),
        WaveKind::Square => Box::new(
            ((((var(pitch) >> square()) | var(cutoff) | dc(q)) >> lowpass::<f32>())
                * (var(gate) >> adsr_live(a, d, s, r)))
                >> pan(0.0),
        ),
        WaveKind::Noise => Box::new(
            ((((white() >> highpass_hz(1500.0_f32, 0.7_f32)) | var(cutoff) | dc(q))
                >> lowpass::<f32>())
                * (var(gate) >> adsr_live(a, d, s, r)))
                >> pan(0.0),
        ),
    };
    let mut unit = unit;
    unit.set_sample_rate(sample_rate);
    unit.allocate();
    unit
}

// ---------------------------------------------------------------------------
// Built-in kit: synthesized PCM one-shots rendered at the engine sample rate
// ---------------------------------------------------------------------------

fn lcg(seed: &mut u64) -> f32 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(37);
    ((*seed >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0
}

fn normalize(buf: &mut [f32]) {
    let peak = buf.iter().map(|x| x.abs()).fold(0.0f32, f32::max).max(1e-6);
    let g = 0.95 / peak;
    for x in buf.iter_mut() {
        *x *= g;
    }
}

fn render_kick(sr: f32) -> Vec<f32> {
    let n = (sr * 0.4) as usize;
    let mut out = vec![0.0; n];
    let mut phase = 0.0;
    let mut seed = 12345;
    for (i, x) in out.iter_mut().enumerate() {
        let t = i as f32 / sr;
        let f = 42.0 + 118.0 * (-t * 30.0).exp();
        phase += 2.0 * std::f32::consts::PI * f / sr;
        *x = phase.sin() * (-t * 8.0).exp();
        if t < 0.012 {
            *x += 0.4 * lcg(&mut seed) * (-t * 400.0).exp();
        }
    }
    normalize(&mut out);
    out
}

fn render_snare(sr: f32) -> Vec<f32> {
    let n = (sr * 0.25) as usize;
    let mut out = vec![0.0; n];
    let mut seed = 777;
    let mut prev = 0.0;
    for (i, x) in out.iter_mut().enumerate() {
        let t = i as f32 / sr;
        let tone = (2.0 * std::f32::consts::PI * 185.0 * t).sin() * (-t * 22.0).exp() * 0.5;
        let nz = lcg(&mut seed);
        let hp = nz - prev;
        prev = nz;
        *x = tone + hp * (-t * 16.0).exp() * 0.55;
    }
    normalize(&mut out);
    out
}

fn render_clap(sr: f32) -> Vec<f32> {
    let n = (sr * 0.3) as usize;
    let mut out = vec![0.0; n];
    let mut seed = 999;
    let mut prev = 0.0;
    for (i, x) in out.iter_mut().enumerate() {
        let t = i as f32 / sr;
        let nz = lcg(&mut seed);
        let hp = nz - prev;
        prev = nz;
        let mut v = 0.0;
        for &tb in &[0.0, 0.012, 0.024] {
            if t >= tb {
                v += (-(t - tb) * 45.0).exp();
            }
        }
        *x = hp * v * 0.5;
    }
    normalize(&mut out);
    out
}

fn render_hat(sr: f32) -> Vec<f32> {
    let n = (sr * 0.09) as usize;
    let mut out = vec![0.0; n];
    let mut seed = 31337;
    let mut prev = 0.0;
    for (i, x) in out.iter_mut().enumerate() {
        let t = i as f32 / sr;
        let nz = lcg(&mut seed);
        let hp = nz - prev;
        prev = nz;
        *x = hp * (-t * 55.0).exp() * 0.5;
    }
    normalize(&mut out);
    out
}

fn render_click(sr: f32, freq: f32) -> Vec<f32> {
    let n = (sr * 0.05) as usize;
    let mut out = vec![0.0; n];
    for (i, x) in out.iter_mut().enumerate() {
        let t = i as f32 / sr;
        *x = (2.0 * std::f32::consts::PI * freq * t).sin() * (-t * 120.0).exp() * 0.6;
    }
    out
}

/// Load an audio file (wav/mp3/ogg/...) into mono f32 + native sample rate.
pub fn load_sample_file(path: &str) -> Result<(Vec<f32>, f32), String> {
    let wave = Wave::load(path).map_err(|e| format!("{e}"))?;
    if wave.is_empty() {
        return Err("empty audio file".into());
    }
    // shortcut: 30s cap per sample; Phase 4 expands to streaming/full stems.
    let n = wave
        .len()
        .min((wave.sample_rate() as usize).saturating_mul(30).max(1));
    let nch = wave.channels().max(1);
    let mut mono = vec![0.0; n];
    for (i, m) in mono.iter_mut().enumerate() {
        let mut v = 0.0;
        for c in 0..nch {
            v += wave.channel(c).get(i).copied().unwrap_or(0.0);
        }
        *m = v / nch as f32;
    }
    Ok((mono, wave.sample_rate() as f32))
}

// ---------------------------------------------------------------------------
// FX: manual stereo feedback delay + fundsp Schroeder reverb send
// ---------------------------------------------------------------------------

struct DelayLine {
    buf: Vec<(f32, f32)>,
    pos: usize,
}

impl DelayLine {
    fn new(sr: f32) -> Self {
        Self {
            buf: vec![(0.0, 0.0); (sr * 2.0) as usize + 1],
            pos: 0,
        }
    }
    fn process(&mut self, sr: f32, time: f32, inl: f32, inr: f32) -> (f32, f32) {
        if self.buf.is_empty() {
            return (0.0, 0.0);
        }
        let tap = ((time.clamp(0.02, 1.9) * sr) as usize).min(self.buf.len() - 1);
        let rp = (self.pos + self.buf.len() - tap) % self.buf.len();
        let (dl, dr) = self.buf[rp];
        let fb = 0.35;
        self.buf[self.pos] = (inl + dl * fb, inr + dr * fb);
        self.pos = (self.pos + 1) % self.buf.len();
        (dl, dr)
    }
}

fn pan_gains(pan: f32, vol: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (vol * angle.cos(), vol * angle.sin())
}

// ---------------------------------------------------------------------------
// Realtime state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum PlayKind {
    Kit(KitPiece),
    Sample(usize),
}

struct PlayHead {
    kind: PlayKind,
    ch: Option<usize>,
    pos: f32,
    pitch_ratio: f32,
    gain: f32,
    lp_z: f32,
    env_sample: usize,
    gate_off: usize,
    rel_sample: usize,
}

impl PlayHead {
    fn new(
        kind: PlayKind,
        ch: Option<usize>,
        pitch_ratio: f32,
        gain: f32,
        gate_off: usize,
    ) -> Self {
        Self {
            kind,
            ch,
            pos: 0.0,
            pitch_ratio,
            gain,
            lp_z: 0.0,
            env_sample: 0,
            gate_off,
            rel_sample: 0,
        }
    }

    fn adsr_gain(&mut self, sr: f32, a: f32, d: f32, s: f32, r: f32) -> (f32, bool) {
        if self.ch.is_none() || self.gate_off == usize::MAX {
            return (1.0, false);
        }
        let a_n = (a.max(0.001) * sr) as usize;
        let d_n = (d.max(0.001) * sr) as usize;
        let r_n = (r.max(0.005) * sr) as usize;
        let s = s.clamp(0.0, 1.0);

        if self.env_sample < self.gate_off {
            let g = if self.env_sample < a_n {
                self.env_sample as f32 / a_n.max(1) as f32
            } else {
                let dt = self.env_sample - a_n;
                if dt < d_n {
                    1.0 - (1.0 - s) * (dt as f32 / d_n.max(1) as f32)
                } else {
                    s
                }
            };
            self.env_sample += 1;
            (g, false)
        } else {
            let t = self.rel_sample as f32 / r_n.max(1) as f32;
            self.rel_sample += 1;
            if t >= 1.0 {
                (0.0, true)
            } else {
                (s * (1.0 - t), false)
            }
        }
    }
}

struct MetroHead {
    accent: bool,
    pos: usize,
}

enum SongEvent {
    NoteOn {
        ch: usize,
        midi: u8,
        vel: f32,
        len_ticks: u32,
    },
    Click {
        accent: bool,
    },
    AudioClip {
        sample: usize,
        gain: f32,
    },
}

struct SmoothCh {
    vol: f32,
    pan: f32,
    cut: f32,
    dly: f32,
    rev: f32,
}

pub struct EngineRt {
    pub sr: f32,
    voices: Vec<Vec<VoiceRt>>,
    playheads: Vec<PlayHead>,
    metro_heads: Vec<MetroHead>,
    kit: [Vec<f32>; 4],
    delay: DelayLine,
    reverb: Box<dyn AudioUnit + Send>,
    metro_acc: Vec<f32>,
    metro_reg: Vec<f32>,
    sample_pos: u64,
    /// Fired-by absolute sample index. Preallocated so block scheduling stays heap-free.
    pending: Vec<(u64, SongEvent)>,
    pub tick_pos: f64,
    pub cur_pat: usize,
    pub cur_tick: u32,
    pub playing: bool,
    pub song_mode: bool,
    pub pattern: usize,
    /// Export helper: stop at arrangement end even when the song loops.
    pub ignore_loop: bool,
    pub peak: f32,
    /// Per-channel post-fader peak, decaying. Feeds the mixer meters.
    pub ch_peaks: Vec<f32>,
    /// Master-bus recording. Captured post-fader while playing.
    pub recording: bool,
    pub take_l: Vec<f32>,
    pub take_r: Vec<f32>,
    /// Set when the take hit the cap (offline/test) or ring filled.
    pub take_full: bool,
    pub take_samples: u64,
    pub xruns: u64,
    pub scope: Vec<f32>,
    scope_pos: usize,
    scope_tick: usize,
    auto_vol: Vec<f32>,
    auto_cut: Vec<Option<f32>>,
    smooth: Vec<SmoothCh>,
    ch_accum: Vec<(f32, f32)>,
    voice_specs: Vec<VoiceSpec>,
    age: u64,
    live_voice: VoiceRt,
    live_off: usize,
    take_tx: Option<rtrb::Producer<f32>>,
}

impl EngineRt {
    pub fn new(sr: f32) -> Self {
        let pitch = shared(midi_hz(69.0_f32));
        let gate = shared(0.0);
        let cutoff = shared(2600.0);
        let live_voice = VoiceRt {
            unit: make_voice(LIVE_VOICE_SPEC, &pitch, &gate, &cutoff, sr as f64),
            pitch,
            gate,
            cutoff,
            vel: 1.0,
            off: 0,
            age: 0,
        };
        let mut reverb: Box<dyn AudioUnit + Send> = Box::new(reverb_stereo(10.0, 2.5, 0.5));
        reverb.set_sample_rate(sr as f64);
        reverb.allocate();
        let mut rt = Self {
            sr,
            voices: Vec::new(),
            playheads: Vec::with_capacity(MAX_PLAYHEADS),
            metro_heads: Vec::with_capacity(8),
            kit: [vec![], vec![], vec![], vec![]],
            delay: DelayLine::new(sr),
            reverb,
            metro_acc: render_click(sr, 2600.0),
            metro_reg: render_click(sr, 2000.0),
            sample_pos: 0,
            pending: Vec::with_capacity(PENDING_CAP),
            tick_pos: 0.0,
            cur_pat: 0,
            cur_tick: 0,
            playing: false,
            song_mode: false,
            pattern: 0,
            ignore_loop: false,
            peak: 0.0,
            ch_peaks: Vec::new(),
            recording: false,
            take_l: Vec::new(),
            take_r: Vec::new(),
            take_full: false,
            take_samples: 0,
            xruns: 0,
            scope: vec![0.0; SCOPE_LEN],
            scope_pos: 0,
            scope_tick: 0,
            auto_vol: Vec::new(),
            auto_cut: Vec::new(),
            smooth: Vec::new(),
            ch_accum: Vec::new(),
            voice_specs: Vec::new(),
            age: 0,
            live_voice,
            live_off: 0,
            take_tx: None,
        };
        rt.render_kit();
        rt
    }

    fn render_kit(&mut self) {
        self.kit = [
            render_kick(self.sr),
            render_snare(self.sr),
            render_clap(self.sr),
            render_hat(self.sr),
        ];
    }

    pub fn set_sample_rate(&mut self, sr: f32, song: &Song) {
        self.sr = sr;
        self.render_kit();
        self.metro_acc = render_click(sr, 2600.0);
        self.metro_reg = render_click(sr, 2000.0);
        self.delay = DelayLine::new(sr);
        let mut reverb: Box<dyn AudioUnit + Send> = Box::new(reverb_stereo(10.0, 2.5, 0.5));
        reverb.set_sample_rate(sr as f64);
        reverb.allocate();
        self.reverb = reverb;
        self.voices.clear();
        self.ensure_channels(song);
        self.reset_transport();
    }

    /// (Re)build voice pools to match the song's channels.
    pub fn ensure_channels(&mut self, song: &Song) {
        if self.voices.len() == song.channels.len() {
            for i in 0..song.channels.len() {
                let spec = song.channels[i].voice_spec();
                if self.voice_specs.get(i) != Some(&spec) {
                    self.rebuild_channel(song, i);
                }
            }
            return;
        }
        self.voices.clear();
        self.smooth.clear();
        self.voice_specs.clear();
        for ch in &song.channels {
            let spec = ch.voice_spec();
            let mut pool = Vec::with_capacity(VOICES_PER_CH);
            for _ in 0..VOICES_PER_CH {
                let pitch = shared(midi_hz(ch.midi_base as f32));
                let gate = shared(0.0);
                let cutoff = shared(ch.cutoff);
                let unit = make_voice(spec, &pitch, &gate, &cutoff, self.sr as f64);
                pool.push(VoiceRt {
                    unit,
                    pitch,
                    gate,
                    cutoff,
                    vel: 1.0,
                    off: 0,
                    age: 0,
                });
            }
            self.voices.push(pool);
            self.voice_specs.push(spec);
            self.smooth.push(SmoothCh {
                vol: ch.volume,
                pan: ch.pan,
                cut: ch.cutoff,
                dly: ch.delay_send,
                rev: ch.reverb_send,
            });
        }
        self.auto_vol = vec![1.0; song.channels.len()];
        self.auto_cut = vec![None; song.channels.len()];
        self.ch_peaks = vec![0.0; song.channels.len()];
        self.ch_accum = vec![(0.0, 0.0); song.channels.len()];
    }

    /// Rebuild one channel's pool (wave / Q / ADSR changed).
    pub fn rebuild_channel(&mut self, song: &Song, idx: usize) {
        if idx >= song.channels.len() {
            return;
        }
        if idx >= self.voices.len() {
            self.ensure_channels(song);
            return;
        }
        let spec = song.channels[idx].voice_spec();
        let cutoff_val = song.channels[idx].cutoff;
        for v in self.voices[idx].iter_mut() {
            v.unit = make_voice(spec, &v.pitch, &v.gate, &v.cutoff, self.sr as f64);
            v.cutoff.set_value(cutoff_val);
            v.off = 0;
        }
        if idx < self.voice_specs.len() {
            self.voice_specs[idx] = spec;
        }
    }

    /// First free voice, else steal the oldest-sounding one.
    fn alloc_voice(&mut self, ch: usize) -> Option<&mut VoiceRt> {
        let pool = self.voices.get_mut(ch)?;
        let idx = pool
            .iter()
            .position(|v| v.off == 0)
            .or_else(|| {
                pool.iter()
                    .enumerate()
                    .min_by_key(|(_, v)| v.age)
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        self.age = self.age.wrapping_add(1);
        let v = &mut pool[idx];
        v.age = self.age;
        Some(v)
    }

    fn push_playhead(&mut self, head: PlayHead) {
        if self.playheads.len() >= MAX_PLAYHEADS {
            self.playheads.remove(0);
        }
        self.playheads.push(head);
    }

    pub fn live_note(&mut self, midi: u8) {
        self.live_voice.pitch.set_value(midi_hz(midi as f32));
        self.live_voice.gate.set_value(1.0);
        self.live_off = (self.sr * 0.4) as usize;
    }

    /// Preview a note using the selected channel's real synth, kit, or pitched sampler.
    pub fn live_note_ch(&mut self, song: &Song, ch: usize, midi: u8, vel: f32) {
        self.ensure_channels(song);
        let Some(channel) = song.channels.get(ch) else {
            self.live_note(midi);
            return;
        };
        let off = (self.sr * 0.35) as usize;
        match channel.kind {
            ChannelKind::Synth { .. } => {
                if let Some(v) = self.alloc_voice(ch) {
                    v.pitch.set_value(midi_hz(midi as f32));
                    v.gate.set_value(1.0);
                    v.vel = vel.clamp(0.05, 1.0);
                    v.off = off;
                }
            }
            ChannelKind::Kit { piece } => {
                self.push_playhead(PlayHead::new(
                    PlayKind::Kit(piece),
                    Some(ch),
                    1.0,
                    vel.clamp(0.05, 1.0),
                    usize::MAX,
                ));
            }
            ChannelKind::Sampler { sample } => {
                if let Some(idx) = sample {
                    let ratio = 2.0_f32.powf((midi as f32 - channel.midi_base as f32) / 12.0);
                    self.push_playhead(PlayHead::new(
                        PlayKind::Sample(idx),
                        Some(ch),
                        ratio,
                        vel.clamp(0.05, 1.0),
                        off,
                    ));
                }
            }
        }
    }

    pub fn live_kit(&mut self, piece: KitPiece) {
        self.push_playhead(PlayHead::new(
            PlayKind::Kit(piece),
            None,
            1.0,
            0.9,
            usize::MAX,
        ));
    }

    pub fn live_sample(&mut self, idx: usize) {
        self.push_playhead(PlayHead::new(
            PlayKind::Sample(idx),
            None,
            1.0,
            0.9,
            usize::MAX,
        ));
    }

    /// Live cutoff control (mixer slider): no graph rebuild needed.
    pub fn set_channel_cutoff(&mut self, idx: usize, v: f32) {
        if let Some(pool) = self.voices.get(idx) {
            for voice in pool.iter() {
                voice.cutoff.set_value(v);
            }
        }
    }

    /// Scope buffer in chronological order for the UI.
    pub fn scope_ordered(&self) -> Vec<f32> {
        let n = self.scope.len();
        (0..n)
            .map(|k| self.scope[(self.scope_pos + k) % n])
            .collect()
    }

    fn pattern_at_tick(&self, song: &Song, abs_tick: u64) -> Option<usize> {
        if self.song_mode {
            let bar = (abs_tick / TICKS_PATTERN as u64) as usize;
            let p = *song.arrangement.get(bar)?;
            if p < 0 { None } else { Some(p as usize) }
        } else {
            Some(self.pattern)
        }
    }

    pub fn process(&mut self, song: &Song, out_l: &mut [f32], out_r: &mut [f32]) {
        self.ensure_channels(song);
        let n = out_l.len().min(out_r.len());
        if n == 0 {
            return;
        }
        let spt = (self.sr * 60.0 / song.bpm.max(30.0) / 16.0).max(1.0) as f64;

        // Collect this block's events on the integer sample clock (no float drift).
        if !self.playing {
            // Stopping drops notes that never started; ringing voices decay.
            self.pending.clear();
        }
        if self.playing {
            let k_start = (self.sample_pos as f64 / spt).ceil() as u64;
            let k_end = ((self.sample_pos + n as u64) as f64 / spt).ceil() as u64;
            for k in k_start..k_end {
                let tick_at = (k as f64 * spt).round() as u64;
                let in_pat = (k % TICKS_PATTERN as u64) as u32;
                if self.song_mode && in_pat == 0 {
                    let bar = (k / TICKS_PATTERN as u64) as usize;
                    for clip in song.audio_clips.iter().filter(|clip| clip.bar == bar) {
                        if self.pending.len() < PENDING_CAP {
                            self.pending.push((
                                tick_at,
                                SongEvent::AudioClip {
                                    sample: clip.sample,
                                    gain: clip.gain,
                                },
                            ));
                        }
                    }
                }
                if let Some(pat) = self.pattern_at_tick(song, k) {
                    self.cur_pat = pat;
                    self.cur_tick = in_pat;
                    let Some(pdata) = song.patterns.get(pat) else {
                        continue;
                    };
                    for ci in 0..song.channels.len() {
                        if let Some(pts) = pdata.auto_vol.get(ci)
                            && let Some(v) = eval_auto(pts, in_pat)
                            && ci < self.auto_vol.len()
                        {
                            self.auto_vol[ci] = v.clamp(0.0, 1.5);
                        }
                        if let Some(pts) = pdata.auto_cut.get(ci)
                            && ci < self.auto_cut.len()
                        {
                            if let Some(v) = eval_auto(pts, in_pat) {
                                let v = v.clamp(40.0, 18000.0);
                                self.auto_cut[ci] = Some(v);
                                if ci < self.voices.len() {
                                    for voice in self.voices[ci].iter() {
                                        voice.cutoff.set_value(v);
                                    }
                                }
                            } else {
                                self.auto_cut[ci] = None;
                            }
                        }
                        if let Some(notes) = pdata.notes.get(ci) {
                            for note in notes.iter().filter(|nn| nn.tick == in_pat) {
                                let mut at = tick_at;
                                if in_pat % 8 == 4 {
                                    at += (song.swing.clamp(0.0, 0.75) as f64 * 2.0 * spt) as u64;
                                }
                                if self.pending.len() < PENDING_CAP {
                                    self.pending.push((
                                        at,
                                        SongEvent::NoteOn {
                                            ch: ci,
                                            midi: note.midi,
                                            vel: note.vel,
                                            len_ticks: note.len.max(1),
                                        },
                                    ));
                                }
                            }
                        }
                    }
                    if song.metronome
                        && in_pat.is_multiple_of(16)
                        && self.pending.len() < PENDING_CAP
                    {
                        self.pending.push((
                            tick_at,
                            SongEvent::Click {
                                accent: in_pat == 0,
                            },
                        ));
                    }
                }
            }
            self.pending.sort_by_key(|(at, _)| *at);
        }

        let mut ei = 0;
        for i in 0..n {
            let now = self.sample_pos + i as u64;
            while ei < self.pending.len() && self.pending[ei].0 <= now {
                match self.pending[ei].1 {
                    SongEvent::NoteOn {
                        ch,
                        midi,
                        vel,
                        len_ticks,
                    } => self.fire_note(song, ch, midi, vel, len_ticks, spt),
                    SongEvent::Click { accent } => {
                        if self.metro_heads.len() < 8 {
                            self.metro_heads.push(MetroHead { accent, pos: 0 });
                        }
                    }
                    SongEvent::AudioClip { sample, gain } => {
                        self.push_playhead(PlayHead::new(
                            PlayKind::Sample(sample),
                            None,
                            1.0,
                            gain.clamp(0.0, 2.0),
                            usize::MAX,
                        ));
                    }
                }
                ei += 1;
            }
            // Envelopes always tick so live preview works while stopped.
            self.tick_gates();
            if self.live_off > 0 {
                self.live_off -= 1;
                if self.live_off == 0 {
                    self.live_voice.gate.set_value(0.0);
                }
            }
            let (l, r) = self.render_sample(song);
            out_l[i] = l;
            out_r[i] = r;
        }
        self.pending.drain(..ei.min(self.pending.len()));
        if self.playing {
            self.sample_pos += n as u64;
            self.tick_pos = self.sample_pos as f64 / spt;
            if self.song_mode && !song.arrangement.is_empty() {
                let total = song.arrangement.len() as u64 * TICKS_PATTERN as u64;
                let end_sample = (total as f64 * spt) as u64;
                if self.sample_pos >= end_sample {
                    if song.loop_song && !self.ignore_loop {
                        // New pass re-collects its own events; drop the old ones.
                        self.sample_pos = 0;
                        self.pending.clear();
                    } else {
                        self.playing = false;
                    }
                }
            }
        }
    }

    fn tick_gates(&mut self) {
        for pool in self.voices.iter_mut() {
            for v in pool.iter_mut() {
                if v.off > 0 {
                    v.off -= 1;
                    if v.off == 0 {
                        v.gate.set_value(0.0);
                    }
                }
            }
        }
    }

    fn fire_note(&mut self, song: &Song, ch: usize, midi: u8, vel: f32, len_ticks: u32, spt: f64) {
        let Some(channel) = song.channels.get(ch) else {
            return;
        };
        let solo_any = song.channels.iter().any(|c| c.solo);
        if solo_any && !channel.solo {
            return;
        }
        if !solo_any && channel.mute {
            return;
        }
        let off = ((len_ticks as f64 * spt) as usize).max(1);
        match channel.kind {
            ChannelKind::Synth { .. } => {
                if ch < self.voices.len()
                    && let Some(v) = self.alloc_voice(ch)
                {
                    v.pitch.set_value(midi_hz(midi as f32));
                    v.gate.set_value(1.0);
                    v.vel = vel.clamp(0.05, 1.0);
                    v.off = off;
                }
            }
            ChannelKind::Kit { piece } => {
                self.push_playhead(PlayHead::new(
                    PlayKind::Kit(piece),
                    Some(ch),
                    1.0,
                    vel,
                    usize::MAX,
                ));
            }
            ChannelKind::Sampler { sample } => {
                if let Some(idx) = sample {
                    let ratio = 2.0_f32.powf((midi as f32 - channel.midi_base as f32) / 12.0);
                    self.push_playhead(PlayHead::new(
                        PlayKind::Sample(idx),
                        Some(ch),
                        ratio,
                        vel,
                        off,
                    ));
                }
            }
        }
    }

    fn render_sample(&mut self, song: &Song) -> (f32, f32) {
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        let mut dly_send = 0.0f32;
        let mut rev_send = 0.0f32;

        for pair in self.ch_accum.iter_mut() {
            *pair = (0.0, 0.0);
        }

        // Accumulate synth voices per channel.
        for (ci, pool) in self.voices.iter_mut().enumerate() {
            let mut vl_sum = 0.0f32;
            let mut vr_sum = 0.0f32;
            for v in pool.iter_mut() {
                let (vl, vr) = v.unit.get_stereo();
                vl_sum += vl * v.vel;
                vr_sum += vr * v.vel;
            }
            if let Some(acc) = self.ch_accum.get_mut(ci) {
                acc.0 += vl_sum;
                acc.1 += vr_sum;
            }
        }

        // Render PCM playheads (kit / pitched sampler / playlist audio clips).
        let sr = self.sr.max(1.0);
        let mut i = 0;
        while i < self.playheads.len() {
            let mut done = false;
            {
                let h = &mut self.playheads[i];
                let (data, base_ratio, sr_ok) = match h.kind {
                    PlayKind::Kit(p) => {
                        let b = match p {
                            KitPiece::Kick => &self.kit[0],
                            KitPiece::Snare => &self.kit[1],
                            KitPiece::Clap => &self.kit[2],
                            KitPiece::Hat => &self.kit[3],
                        };
                        (b.as_slice(), 1.0, true)
                    }
                    PlayKind::Sample(idx) => match song.samples.get(idx) {
                        Some(s) if !s.data.is_empty() && s.sr > 0.0 => {
                            (s.data.as_ref(), s.sr / sr, true)
                        }
                        _ => (&[][..], 1.0, false),
                    },
                };
                if !sr_ok || (h.pos as usize) >= data.len() {
                    done = true;
                } else {
                    let p0 = h.pos as usize;
                    let frac = h.pos - p0 as f32;
                    let v0 = data[p0];
                    let v1 = data.get(p0 + 1).copied().unwrap_or(0.0);
                    let mut v = (v0 + (v1 - v0) * frac) * h.gain;

                    if let Some(ci) = h.ch
                        && let Some(channel) = song.channels.get(ci)
                    {
                        let cut = self
                            .auto_cut
                            .get(ci)
                            .and_then(|x| *x)
                            .unwrap_or(channel.cutoff);
                        let alpha = (2.0 * std::f32::consts::PI * cut.clamp(40.0, 18000.0) / sr)
                            .clamp(0.01, 0.99);
                        h.lp_z += alpha * (v - h.lp_z);
                        v = h.lp_z;

                        let (env_g, env_done) = h.adsr_gain(
                            sr,
                            channel.attack,
                            channel.decay,
                            channel.sustain,
                            channel.release,
                        );
                        v *= env_g;
                        if env_done {
                            done = true;
                        }
                        if let Some(acc) = self.ch_accum.get_mut(ci) {
                            acc.0 += v;
                            acc.1 += v;
                        }
                    } else {
                        l += v * 0.9;
                        r += v * 0.9;
                    }
                    h.pos += base_ratio * h.pitch_ratio;
                }
            }
            if done {
                self.playheads.swap_remove(i);
            } else {
                i += 1;
            }
        }

        // Smooth channel controls, pan, mix, meter, and accumulate sends.
        let smooth_k = (220.0 / sr).clamp(0.001, 0.25);
        let solo_any = song.channels.iter().any(|c| c.solo);
        for (ci, channel) in song.channels.iter().enumerate() {
            let (raw_l, raw_r) = self.ch_accum.get(ci).copied().unwrap_or((0.0, 0.0));
            let sm = self.smooth.get_mut(ci);
            let (vol_s, pan_s, dly_s, rev_s) = if let Some(s) = sm {
                s.vol += (channel.volume - s.vol) * smooth_k;
                s.pan += (channel.pan - s.pan) * smooth_k;
                s.cut += (channel.cutoff - s.cut) * smooth_k;
                s.dly += (channel.delay_send - s.dly) * smooth_k;
                s.rev += (channel.reverb_send - s.rev) * smooth_k;
                (s.vol, s.pan, s.dly, s.rev)
            } else {
                (
                    channel.volume,
                    channel.pan,
                    channel.delay_send,
                    channel.reverb_send,
                )
            };

            let muted = (solo_any && !channel.solo) || (!solo_any && channel.mute);
            let vol_auto = self.auto_vol.get(ci).copied().unwrap_or(1.0);
            let fader = if muted { 0.0 } else { vol_s * vol_auto };
            let (lg, rg) = pan_gains(pan_s, fader);
            let cl = raw_l * lg;
            let cr = raw_r * rg;
            l += cl;
            r += cr;
            let mono = (cl + cr) * 0.5;
            dly_send += mono * dly_s;
            rev_send += mono * rev_s;
            if ci < self.ch_peaks.len() {
                let p = cl.abs().max(cr.abs());
                self.ch_peaks[ci] = (self.ch_peaks[ci] * 0.94).max(p);
            }
        }

        {
            let (vl, vr) = self.live_voice.unit.get_stereo();
            l += vl * 0.8;
            r += vr * 0.8;
        }

        // Metronome overlay.
        let mut i = 0;
        while i < self.metro_heads.len() {
            let done = {
                let h = &mut self.metro_heads[i];
                let buf = if h.accent {
                    &self.metro_acc
                } else {
                    &self.metro_reg
                };
                if h.pos >= buf.len() {
                    true
                } else {
                    let v = buf[h.pos] * 0.5;
                    l += v;
                    r += v;
                    h.pos += 1;
                    false
                }
            };
            if done {
                self.metro_heads.swap_remove(i);
            } else {
                i += 1;
            }
        }

        // Sends: manual stereo delay + fundsp Schroeder reverb.
        let (dl, dr) = self
            .delay
            .process(self.sr, song.delay_time, dly_send, dly_send);
        let (rl, rr) = self.reverb.filter_stereo(rev_send, rev_send);
        l += dl * song.delay_mix + rl * song.reverb_mix;
        r += dr * song.delay_mix + rr * song.reverb_mix;

        l *= song.master_vol * 0.22;
        r *= song.master_vol * 0.22;
        l = l.tanh();
        r = r.tanh();
        if self.recording && self.playing {
            if let Some(tx) = &mut self.take_tx {
                if tx.slots() >= 2 {
                    let _ = tx.push(l);
                    let _ = tx.push(r);
                    self.take_samples = self.take_samples.saturating_add(1);
                } else {
                    self.xruns = self.xruns.saturating_add(1);
                }
            } else if self.take_l.len() >= 44100 * 60 * 5 {
                self.recording = false;
                self.take_full = true;
            } else {
                self.take_l.push(l);
                self.take_r.push(r);
                self.take_samples = self.take_l.len() as u64;
            }
        }
        self.peak = (self.peak * 0.9995).max(l.abs().max(r.abs()));
        self.scope_tick += 1;
        if self.scope_tick.is_multiple_of(4) {
            self.scope[self.scope_pos] = (l + r) * 0.5;
            self.scope_pos = (self.scope_pos + 1) % self.scope.len();
        }
        (l, r)
    }

    pub fn reset_transport(&mut self) {
        self.sample_pos = 0;
        self.tick_pos = 0.0;
        self.cur_tick = 0;
        for pool in self.voices.iter_mut() {
            for v in pool.iter_mut() {
                v.gate.set_value(0.0);
                v.off = 0;
            }
        }
        self.live_voice.gate.set_value(0.0);
        self.live_off = 0;
        self.playheads.clear();
        self.metro_heads.clear();
        self.pending.clear();
        self.recording = false;
        for p in self.ch_peaks.iter_mut() {
            *p = 0.0;
        }
    }
}

// ---------------------------------------------------------------------------
// Lock-free audio bridge (UI thread <-> cpal audio thread <-> take writer)
// ---------------------------------------------------------------------------

enum AudioCmd {
    SetSong(Arc<Song>),
    SetPlaying(bool),
    SetSongMode(bool),
    SetPattern(usize),
    ResetTransport,
    SetRecording(bool),
    ClearTake,
    RebuildChannel(usize),
    SetChannelCutoff(usize, f32),
    LiveNote { ch: usize, midi: u8, vel: f32 },
    LiveKit(KitPiece),
    LiveSample(usize),
}

struct AudioTelemetry {
    playing: AtomicBool,
    recording: AtomicBool,
    take_full: AtomicBool,
    cur_pat: AtomicUsize,
    cur_tick: AtomicU32,
    tick_pos_bits: AtomicU64,
    peak_bits: AtomicU32,
    xruns: AtomicU64,
    take_samples: AtomicU64,
    ack_seq: AtomicU64,
    ch_peaks: [AtomicU32; MAX_TELEM_CH],
    scope: [AtomicU32; SCOPE_LEN],
}

impl AudioTelemetry {
    fn new() -> Self {
        Self {
            playing: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            take_full: AtomicBool::new(false),
            cur_pat: AtomicUsize::new(0),
            cur_tick: AtomicU32::new(0),
            tick_pos_bits: AtomicU64::new(0.0_f64.to_bits()),
            peak_bits: AtomicU32::new(0.0_f32.to_bits()),
            xruns: AtomicU64::new(0),
            take_samples: AtomicU64::new(0),
            ack_seq: AtomicU64::new(0),
            ch_peaks: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
            scope: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
        }
    }
}

struct TakeDiskHandle {
    path: PathBuf,
    clear_flag: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    written_samples: Arc<AtomicU64>,
}

impl Drop for TakeDiskHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
    }
}

fn spawn_take_writer(mut take_rx: rtrb::Consumer<f32>) -> TakeDiskHandle {
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("thunder-take-{pid}.raw"));
    let clear_flag = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let written_samples = Arc::new(AtomicU64::new(0));

    let p = path.clone();
    let clr = Arc::clone(&clear_flag);
    let stop = Arc::clone(&stop_flag);
    let written = Arc::clone(&written_samples);

    std::thread::Builder::new()
        .name("thunder-take-writer".into())
        .spawn(move || {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&p)
                .ok();
            let mut buf = [0.0f32; 2048];
            while !stop.load(Ordering::Acquire) {
                if clr.swap(false, Ordering::AcqRel) {
                    while take_rx.pop().is_ok() {}
                    if let Some(f) = file.as_mut() {
                        let _ = f.set_len(0);
                        let _ = f.seek(SeekFrom::Start(0));
                    }
                    written.store(0, Ordering::Release);
                }
                let avail = take_rx.slots();
                let frames = avail / 2;
                if frames == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(8));
                    continue;
                }
                let take_n = (frames * 2).min(buf.len());
                let mut got = 0;
                for slot in buf[..take_n].iter_mut() {
                    match take_rx.pop() {
                        Ok(s) => {
                            *slot = s;
                            got += 1;
                        }
                        Err(_) => break,
                    }
                }
                if got >= 2 {
                    let even = got - (got % 2);
                    if let Some(f) = file.as_mut() {
                        let bytes = unsafe {
                            std::slice::from_raw_parts(buf.as_ptr() as *const u8, even * 4)
                        };
                        if f.write_all(bytes).is_ok() {
                            written.fetch_add((even / 2) as u64, Ordering::Relaxed);
                        }
                    }
                }
            }
            let _ = std::fs::remove_file(&p);
        })
        .ok();

    TakeDiskHandle {
        path,
        clear_flag,
        stop_flag,
        written_samples,
    }
}

pub struct AudioBridge {
    cmd_tx: rtrb::Producer<AudioCmd>,
    recycle_rx: rtrb::Consumer<Arc<Song>>,
    telemetry: Arc<AudioTelemetry>,
    last_sent: Song,
    sent_seq: u64,
    take_disk: TakeDiskHandle,
}

impl AudioBridge {
    fn send(&mut self, cmd: AudioCmd) {
        if self.cmd_tx.push(cmd).is_ok() {
            self.sent_seq = self.sent_seq.wrapping_add(1);
        } else {
            self.telemetry.xruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct AudioWorker {
    song: Arc<Song>,
    rt: EngineRt,
    cmd_rx: rtrb::Consumer<AudioCmd>,
    recycle_tx: rtrb::Producer<Arc<Song>>,
    telemetry: Arc<AudioTelemetry>,
    bl: Vec<f32>,
    br: Vec<f32>,
}

impl AudioWorker {
    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.pop() {
            match cmd {
                AudioCmd::SetSong(new_song) => {
                    let old = std::mem::replace(&mut self.song, new_song);
                    let _ = self.recycle_tx.push(old);
                    self.rt.ensure_channels(&self.song);
                }
                AudioCmd::SetPlaying(p) => self.rt.playing = p,
                AudioCmd::SetSongMode(m) => self.rt.song_mode = m,
                AudioCmd::SetPattern(pat) => self.rt.pattern = pat,
                AudioCmd::ResetTransport => self.rt.reset_transport(),
                AudioCmd::SetRecording(r) => self.rt.recording = r,
                AudioCmd::ClearTake => {
                    self.rt.take_full = false;
                    self.rt.take_samples = 0;
                }
                AudioCmd::RebuildChannel(ch) => self.rt.rebuild_channel(&self.song, ch),
                AudioCmd::SetChannelCutoff(ch, v) => self.rt.set_channel_cutoff(ch, v),
                AudioCmd::LiveNote { ch, midi, vel } => {
                    self.rt.live_note_ch(&self.song, ch, midi, vel);
                }
                AudioCmd::LiveKit(p) => self.rt.live_kit(p),
                AudioCmd::LiveSample(idx) => self.rt.live_sample(idx),
            }
            self.telemetry.ack_seq.fetch_add(1, Ordering::Release);
        }
    }

    fn process_block<T>(&mut self, data: &mut [T], channels: usize)
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        if channels == 0 {
            return;
        }
        self.drain_commands();
        let frames = data.len() / channels;
        if frames > self.bl.len() {
            self.bl.resize(frames, 0.0);
            self.br.resize(frames, 0.0);
        }
        self.rt
            .process(&self.song, &mut self.bl[..frames], &mut self.br[..frames]);
        for (f, chunk) in data.chunks_mut(channels).enumerate() {
            for (c, x) in chunk.iter_mut().enumerate() {
                let s = if c % 2 == 0 { self.bl[f] } else { self.br[f] };
                *x = T::from_sample(s);
            }
        }
        self.publish_telemetry();
    }

    fn publish_telemetry(&self) {
        let t = &self.telemetry;
        t.playing.store(self.rt.playing, Ordering::Relaxed);
        t.recording.store(self.rt.recording, Ordering::Relaxed);
        t.take_full.store(self.rt.take_full, Ordering::Relaxed);
        t.cur_pat.store(self.rt.cur_pat, Ordering::Relaxed);
        t.cur_tick.store(self.rt.cur_tick, Ordering::Relaxed);
        t.tick_pos_bits
            .store(self.rt.tick_pos.to_bits(), Ordering::Relaxed);
        t.peak_bits.store(self.rt.peak.to_bits(), Ordering::Relaxed);
        t.take_samples
            .store(self.rt.take_samples, Ordering::Relaxed);
        if self.rt.xruns > 0 {
            t.xruns.store(self.rt.xruns, Ordering::Relaxed);
        }
        for (i, p) in self.rt.ch_peaks.iter().take(MAX_TELEM_CH).enumerate() {
            t.ch_peaks[i].store(p.to_bits(), Ordering::Relaxed);
        }
        let n = self.rt.scope.len().min(SCOPE_LEN);
        for k in 0..n {
            let v = self.rt.scope[(self.rt.scope_pos + k) % n];
            t.scope[k].store(v.to_bits(), Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// App state + cpal backend
// ---------------------------------------------------------------------------

pub struct AppState {
    pub song: Song,
    pub rt: EngineRt,
    pub audio: Option<AudioBridge>,
}

impl AppState {
    pub fn new() -> Self {
        let song = crate::song::new_empty();
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        Self {
            song,
            rt,
            audio: None,
        }
    }

    /// Push song edits to the lock-free audio thread and pull telemetry back.
    pub fn sync_audio(&mut self) {
        let Some(bridge) = &mut self.audio else {
            return;
        };
        while bridge.recycle_rx.pop().is_ok() {}
        if bridge.last_sent != self.song {
            let snap = Arc::new(self.song.clone());
            if bridge.cmd_tx.push(AudioCmd::SetSong(snap)).is_ok() {
                bridge.sent_seq = bridge.sent_seq.wrapping_add(1);
                bridge.last_sent = self.song.clone();
            }
        }
        let t = &bridge.telemetry;
        let synced = t.ack_seq.load(Ordering::Acquire) >= bridge.sent_seq;
        if synced {
            self.rt.playing = t.playing.load(Ordering::Relaxed);
            self.rt.recording = t.recording.load(Ordering::Relaxed);
            self.rt.take_full = t.take_full.load(Ordering::Relaxed);
        }
        self.rt.cur_pat = t.cur_pat.load(Ordering::Relaxed);
        self.rt.cur_tick = t.cur_tick.load(Ordering::Relaxed);
        self.rt.tick_pos = f64::from_bits(t.tick_pos_bits.load(Ordering::Relaxed));
        self.rt.peak = f32::from_bits(t.peak_bits.load(Ordering::Relaxed));
        self.rt.xruns = t.xruns.load(Ordering::Relaxed);
        self.rt.take_samples = t.take_samples.load(Ordering::Relaxed);

        let nch = self.song.channels.len().min(MAX_TELEM_CH);
        if self.rt.ch_peaks.len() != self.song.channels.len() {
            self.rt.ch_peaks.resize(self.song.channels.len(), 0.0);
        }
        for i in 0..nch {
            self.rt.ch_peaks[i] = f32::from_bits(t.ch_peaks[i].load(Ordering::Relaxed));
        }
        for (i, s) in self.rt.scope.iter_mut().enumerate().take(SCOPE_LEN) {
            *s = f32::from_bits(t.scope[i].load(Ordering::Relaxed));
        }
        self.rt.scope_pos = 0;
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.rt.playing = playing;
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetPlaying(playing));
        }
    }

    pub fn set_song_mode(&mut self, song_mode: bool) {
        self.rt.song_mode = song_mode;
        self.rt.reset_transport();
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetSongMode(song_mode));
            b.send(AudioCmd::ResetTransport);
        }
    }

    pub fn set_pattern(&mut self, pat: usize) {
        self.rt.pattern = pat;
        self.rt.reset_transport();
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetPattern(pat));
            b.send(AudioCmd::ResetTransport);
        }
    }

    pub fn select_pattern(&mut self, pat: usize) {
        self.rt.pattern = pat;
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetPattern(pat));
        }
    }

    pub fn reset_transport(&mut self) {
        self.rt.reset_transport();
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::ResetTransport);
        }
    }

    pub fn set_recording(&mut self, rec: bool, clear_existing: bool) {
        if clear_existing {
            self.rt.take_l.clear();
            self.rt.take_r.clear();
            self.rt.take_full = false;
            self.rt.take_samples = 0;
            if let Some(b) = &mut self.audio {
                b.take_disk.clear_flag.store(true, Ordering::Release);
                b.send(AudioCmd::ClearTake);
            }
        }
        if rec {
            self.rt.playing = true;
            if let Some(b) = &mut self.audio {
                b.send(AudioCmd::SetPlaying(true));
            }
        }
        self.rt.recording = rec;
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetRecording(rec));
        }
    }

    pub fn apply_loaded_song(&mut self, song: Song) {
        self.song = song;
        let sr = self.rt.sr;
        self.rt.set_sample_rate(sr, &self.song);
        self.sync_audio();
    }

    pub fn rebuild_channel(&mut self, ch: usize) {
        self.rt.rebuild_channel(&self.song, ch);
        if let Some(b) = &mut self.audio {
            let snap = Arc::new(self.song.clone());
            b.send(AudioCmd::SetSong(snap));
            b.last_sent = self.song.clone();
            b.send(AudioCmd::RebuildChannel(ch));
        }
    }

    pub fn set_channel_cutoff(&mut self, ch: usize, v: f32) {
        self.rt.set_channel_cutoff(ch, v);
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetChannelCutoff(ch, v));
        }
    }

    pub fn live_note_ch(&mut self, ch: usize, midi: u8) {
        self.rt.live_note_ch(&self.song, ch, midi, 0.85);
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::LiveNote {
                ch,
                midi,
                vel: 0.85,
            });
        }
    }

    pub fn live_kit(&mut self, piece: KitPiece) {
        self.rt.live_kit(piece);
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::LiveKit(piece));
        }
    }

    pub fn live_sample(&mut self, idx: usize) {
        self.rt.live_sample(idx);
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::LiveSample(idx));
        }
    }

    pub fn take_duration_secs(&self) -> usize {
        let n = if let Some(b) = &self.audio {
            b.take_disk.written_samples.load(Ordering::Relaxed)
        } else {
            self.rt.take_l.len() as u64
        };
        (n / self.rt.sr.max(1.0) as u64) as usize
    }

    pub fn has_take(&self) -> bool {
        if let Some(b) = &self.audio {
            b.take_disk.written_samples.load(Ordering::Relaxed) > 0
        } else {
            !self.rt.take_l.is_empty()
        }
    }

    pub fn save_take(&self, path: &str) -> Result<usize, String> {
        let Some(b) = &self.audio else {
            return crate::export::write_wav_file(
                path,
                &self.rt.take_l,
                &self.rt.take_r,
                self.rt.sr,
            );
        };
        let n_frames = b.take_disk.written_samples.load(Ordering::Relaxed) as usize;
        if n_frames == 0 {
            return Err("nothing recorded yet".into());
        }
        let mut raw = std::fs::File::open(&b.take_disk.path).map_err(|e| e.to_string())?;
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: self.rt.sr as u32,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
        let mut bytes = [0u8; 8192];
        let mut written = 0usize;
        let max_bytes = n_frames * 8;
        while written < max_bytes {
            let want = (max_bytes - written).min(bytes.len());
            let read_n = raw.read(&mut bytes[..want]).map_err(|e| e.to_string())?;
            if read_n < 8 {
                break;
            }
            let (chunks, _) = bytes[..read_n].as_chunks::<8>();
            for frame in chunks {
                let l = f32::from_ne_bytes([frame[0], frame[1], frame[2], frame[3]]);
                let r = f32::from_ne_bytes([frame[4], frame[5], frame[6], frame[7]]);
                w.write_sample(l).map_err(|e| e.to_string())?;
                w.write_sample(r).map_err(|e| e.to_string())?;
                written += 8;
            }
        }
        w.finalize().map_err(|e| e.to_string())?;
        Ok(written / 8)
    }
}

/// Load a file into the song's sample bank (deduplicated by path).
pub fn sample_into_song(s: &mut AppState, path: &str) -> Result<usize, String> {
    if path.trim().is_empty() {
        return Err("empty path".into());
    }
    if let Some(idx) = s.song.samples.iter().position(|x| x.path == path) {
        return Ok(idx);
    }
    let (data, sr) = load_sample_file(path)?;
    let name = std::path::Path::new(path)
        .file_name()
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    s.song.samples.push(crate::song::SampleRef {
        name,
        path: path.into(),
        data: data.into(),
        sr,
    });
    Ok(s.song.samples.len() - 1)
}

pub fn start_audio(state: &mut AppState) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let supported = device.default_output_config().ok()?;
    let sr = supported.sample_rate() as f32;

    state.rt.set_sample_rate(sr, &state.song);

    let (cmd_tx, cmd_rx) = rtrb::RingBuffer::<AudioCmd>::new(256);
    let (recycle_tx, recycle_rx) = rtrb::RingBuffer::<Arc<Song>>::new(64);
    let (take_tx, take_rx) = rtrb::RingBuffer::<f32>::new(TAKE_RING_FLOATS);

    let telemetry = Arc::new(AudioTelemetry::new());
    let take_disk = spawn_take_writer(take_rx);

    let mut worker_rt = EngineRt::new(sr);
    worker_rt.take_tx = Some(take_tx);
    worker_rt.ensure_channels(&state.song);

    let mut worker = AudioWorker {
        song: Arc::new(state.song.clone()),
        rt: worker_rt,
        cmd_rx,
        recycle_tx,
        telemetry: Arc::clone(&telemetry),
        bl: vec![0.0; 4096],
        br: vec![0.0; 4096],
    };

    let config: cpal::StreamConfig = supported.config();
    let channels = config.channels as usize;
    let err_fn = |err| eprintln!("audio stream error: {err}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| worker.process_block(data, channels),
                err_fn,
                None,
            )
            .ok()?,
        cpal::SampleFormat::I16 => device
            .build_output_stream(
                config,
                move |data: &mut [i16], _| worker.process_block(data, channels),
                err_fn,
                None,
            )
            .ok()?,
        cpal::SampleFormat::U16 => device
            .build_output_stream(
                config,
                move |data: &mut [u16], _| worker.process_block(data, channels),
                err_fn,
                None,
            )
            .ok()?,
        _ => return None,
    };
    stream.play().ok()?;

    state.audio = Some(AudioBridge {
        cmd_tx,
        recycle_rx,
        telemetry,
        last_sent: state.song.clone(),
        sent_seq: 0,
        take_disk,
    });
    Some(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: a fresh engine (all gates closed) must be bit-finite.
    /// A zeroed filter default once NaN'd the SVF and poisoned the mix bus.
    #[test]
    fn fresh_engine_at_rest_is_finite() {
        let song = crate::song::demo_song();
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        for _ in 0..44100 {
            let (l, r) = rt.render_sample(&song);
            assert!(l.is_finite() && r.is_finite(), "NaN at rest");
        }
    }

    #[test]
    fn scheduler_fires_demo_kick() {
        let song = crate::song::demo_song();
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        assert_eq!(rt.voices.len(), song.channels.len());
        rt.song_mode = true;
        rt.playing = true;
        let mut l = vec![0.0; 44100];
        let mut r = vec![0.0; 44100];
        rt.process(&song, &mut l, &mut r);
        assert!(rt.tick_pos > 10.0, "sequencer did not advance");
        let peak = l
            .iter()
            .chain(r.iter())
            .map(|x| x.abs())
            .fold(0.0f32, f32::max);
        assert!(peak > 0.01, "silent engine, peak={peak}");
        assert!(
            l.iter().chain(r.iter()).all(|x| x.is_finite()),
            "NaN in output"
        );
        assert!(
            rt.ch_peaks[0] > 0.01,
            "kick channel meter must reflect routed kit audio"
        );
    }

    #[test]
    fn kit_channel_respects_mute_and_volume() {
        let mut song = crate::song::new_empty();
        song.patterns[0].notes[0].push(crate::song::Note::new(0, 4, 36, 1.0));
        song.channels[0].mute = true;
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        rt.playing = true;
        let mut l = vec![0.0; 4096];
        let mut r = vec![0.0; 4096];
        rt.process(&song, &mut l, &mut r);
        let peak = l.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        assert!(peak < 1e-5, "muted kit channel must stay silent");
    }

    #[test]
    fn sampler_pitches_relative_to_midi_base() {
        let mut song = crate::song::new_empty();
        song.samples.push(crate::song::SampleRef {
            name: "sine.wav".into(),
            path: "sine.wav".into(),
            data: (0..4410)
                .map(|i| (i as f32 * 0.1).sin())
                .collect::<Vec<_>>()
                .into(),
            sr: 44100.0,
        });
        song.channels[7].kind = ChannelKind::Sampler { sample: Some(0) };
        song.channels[7].midi_base = 60;
        let mut rt = EngineRt::new(44100.0);
        rt.live_note_ch(&song, 7, 72, 1.0); // +1 octave -> ratio 2.0
        assert_eq!(rt.playheads.len(), 1);
        assert!((rt.playheads[0].pitch_ratio - 2.0).abs() < 1e-4);
    }

    #[test]
    fn lock_free_worker_handles_rapid_song_swaps() {
        let song = crate::song::demo_song();
        let (mut cmd_tx, cmd_rx) = rtrb::RingBuffer::<AudioCmd>::new(64);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::<Arc<Song>>::new(64);
        let telemetry = Arc::new(AudioTelemetry::new());
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        let mut worker = AudioWorker {
            song: Arc::new(song.clone()),
            rt,
            cmd_rx,
            recycle_tx,
            telemetry: Arc::clone(&telemetry),
            bl: vec![0.0; 512],
            br: vec![0.0; 512],
        };
        cmd_tx.push(AudioCmd::SetSongMode(true)).unwrap();
        cmd_tx.push(AudioCmd::SetPlaying(true)).unwrap();
        for i in 0..20 {
            let mut s = song.clone();
            s.bpm = 120.0 + i as f32;
            cmd_tx.push(AudioCmd::SetSong(Arc::new(s))).unwrap();
            let mut out = [0.0f32; 512];
            worker.process_block(&mut out, 2);
            while recycle_rx.pop().is_ok() {}
        }
        assert!(telemetry.playing.load(Ordering::Relaxed));
        assert!(f64::from_bits(telemetry.tick_pos_bits.load(Ordering::Relaxed)) > 0.0);
    }

    #[test]
    fn take_records_while_playing() {
        let song = crate::song::demo_song();
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        rt.song_mode = true;
        rt.playing = true;
        rt.recording = true;
        let mut l = vec![0.0; 44100];
        let mut r = vec![0.0; 44100];
        rt.process(&song, &mut l, &mut r);
        assert_eq!(rt.take_l.len(), 44100, "one second of take");
        assert!(rt.take_l.iter().all(|x| x.is_finite()));
        rt.playing = false;
        rt.process(&song, &mut l, &mut r);
        assert_eq!(rt.take_l.len(), 44100, "stopped transport adds nothing");
        let path = std::env::temp_dir().join("thunder-take-test.wav");
        let path = path.to_str().unwrap();
        let n = crate::export::write_wav_file(path, &rt.take_l, &rt.take_r, 44100.0).unwrap();
        assert_eq!(n, 44100);
        let reader = hound::WavReader::open(path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.len(), 44100 * 2);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn playlist_audio_clip_renders() {
        let mut song = crate::song::new_empty();
        song.arrangement.push(-1);
        song.samples.push(crate::song::SampleRef {
            name: "tone.wav".into(),
            path: "tone.wav".into(),
            data: vec![0.5; 4410].into(),
            sr: 44100.0,
        });
        song.audio_clips.push(crate::song::AudioClip {
            bar: 0,
            sample: 0,
            gain: 1.0,
        });
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        rt.song_mode = true;
        rt.playing = true;
        let mut l = vec![0.0; 1024];
        let mut r = vec![0.0; 1024];
        rt.process(&song, &mut l, &mut r);

        assert!(l.iter().any(|sample| sample.abs() > 0.01));
        assert!(l.iter().chain(&r).all(|sample| sample.is_finite()));
    }
}
