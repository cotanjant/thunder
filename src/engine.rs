//! Realtime engine: lock-free cpal backend, fundsp synth voices (polyphonic pools),
//! PCM kit + pitched sampler playback routed through per-channel DSP,
//! delay/reverb sends, sample-clock scheduler, and disk-streaming take recorder.

use crate::song::{
    AutoTarget, ChannelKind, ClipKind, KitPiece, Song, VoiceSpec, WaveKind, eval_auto,
    eval_auto_clip,
};
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
    pan: f32,
    cur_hz: f32,
    target_hz: f32,
    glide_rate: f32,
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
    // adsr_live only arms after seeing at least one sample with gate <= 0.0;
    // prime it once so a note starting on tick 0 triggers cleanly.
    gate.set_value(0.0);
    let _ = unit.get_stereo();
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
    mixer_insert: usize,
    pos: f32,
    end_pos: Option<f32>,
    pitch_ratio: f32,
    gain: f32,
    pan: f32,
    fade_in_samples: usize,
    fade_out_samples: usize,
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
            mixer_insert: 0,
            pos: 0.0,
            end_pos: None,
            pitch_ratio,
            gain,
            pan: 0.0,
            fade_in_samples: 0,
            fade_out_samples: 0,
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

struct NoteTrigger {
    ch: usize,
    midi: u8,
    vel: f32,
    pan: f32,
    fine_pitch: f32,
    len_ticks: u32,
}

enum SongEvent {
    NoteOn {
        ch: usize,
        midi: u8,
        vel: f32,
        pan: f32,
        fine_pitch: f32,
        len_ticks: u32,
    },
    Click {
        accent: bool,
    },
    AudioClip {
        sample: usize,
        gain: f32,
        pitch_semitones: f32,
        offset_ticks: u32,
        len_ticks: u32,
        fade_in_ticks: u32,
        fade_out_ticks: u32,
        mixer_insert: usize,
    },
}

struct SmoothCh {
    vol: f32,
    pan: f32,
    cut: f32,
    dly: f32,
    rev: f32,
}

struct SmoothMix {
    vol: f32,
    pan: f32,
    dly: f32,
    rev: f32,
}

#[derive(Clone, Default)]
struct RingDelay {
    buf: Vec<(f32, f32)>,
    pos: usize,
}

impl RingDelay {
    fn reserve(&mut self, max_delay: usize) {
        let need = max_delay.saturating_add(1).max(1);
        if self.buf.len() < need {
            self.buf.resize(need, (0.0, 0.0));
            self.pos = 0;
        }
    }

    fn push_pop(&mut self, input: (f32, f32), delay_samples: usize) -> (f32, f32) {
        if delay_samples == 0 || self.buf.len() <= delay_samples {
            return input;
        }
        let len = self.buf.len();
        let rp = (self.pos + len - delay_samples) % len;
        let out = self.buf[rp];
        self.buf[self.pos] = input;
        self.pos = (self.pos + 1) % len;
        out
    }

    fn clear(&mut self) {
        self.buf.fill((0.0, 0.0));
        self.pos = 0;
    }
}

fn mixer_graph_sig(song: &Song) -> u64 {
    let mut h = (song.mixer_tracks.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for (i, t) in song.mixer_tracks.iter().enumerate() {
        h ^= ((t.latency_samples as u64) << 16) ^ (i as u64);
        h = h.rotate_left(7);
        for r in &t.routes {
            h ^= ((r.target as u64) << 8) ^ (r.sidechain_only as u64);
            h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        }
    }
    h
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
    /// Per-mixer-insert post-fader peak, decaying.
    pub mix_peaks: Vec<f32>,
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
    mix_auto_vol: Vec<f32>,
    master_auto_vol: Option<f32>,
    smooth: Vec<SmoothCh>,
    mix_smooth: Vec<SmoothMix>,
    ch_accum: Vec<(f32, f32)>,
    mix_direct_in: Vec<(f32, f32)>,
    mix_route_in: Vec<(f32, f32)>,
    track_lat_delay: Vec<RingDelay>,
    direct_pdc_delay: Vec<RingDelay>,
    route_pdc_delay: Vec<Vec<RingDelay>>,
    pdc_order: Vec<usize>,
    pdc_direct_delay: Vec<usize>,
    pdc_route_delay: Vec<Vec<(usize, usize)>>,
    mixer_sig: u64,
    voice_specs: Vec<VoiceSpec>,
    channel_last_hz: Vec<Option<f32>>,
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
            pan: 0.0,
            cur_hz: midi_hz(69.0_f32),
            target_hz: midi_hz(69.0_f32),
            glide_rate: 1.0,
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
            mix_peaks: Vec::new(),
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
            mix_auto_vol: Vec::new(),
            master_auto_vol: None,
            smooth: Vec::new(),
            mix_smooth: Vec::new(),
            ch_accum: Vec::new(),
            mix_direct_in: Vec::new(),
            mix_route_in: Vec::new(),
            track_lat_delay: Vec::new(),
            direct_pdc_delay: Vec::new(),
            route_pdc_delay: Vec::new(),
            pdc_order: Vec::new(),
            pdc_direct_delay: Vec::new(),
            pdc_route_delay: Vec::new(),
            mixer_sig: u64::MAX,
            voice_specs: Vec::new(),
            channel_last_hz: Vec::new(),
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
        self.mixer_sig = u64::MAX;
        self.ensure_channels(song);
        self.reset_transport();
    }

    fn ensure_mixer_graph(&mut self, song: &Song) {
        let sig = mixer_graph_sig(song);
        let nmix = song.mixer_tracks.len();
        if self.mixer_sig == sig && self.mix_direct_in.len() == nmix {
            return;
        }
        self.mixer_sig = sig;
        let pdc = song.compute_pdc();
        self.pdc_order = pdc.order;
        self.pdc_direct_delay = pdc.direct_delay;
        self.pdc_route_delay = pdc.route_delay;

        self.mix_peaks.resize(nmix, 0.0);
        self.mix_auto_vol.resize(nmix, 1.0);
        self.mix_direct_in.resize(nmix, (0.0, 0.0));
        self.mix_route_in.resize(nmix, (0.0, 0.0));
        self.mix_smooth.clear();
        for t in &song.mixer_tracks {
            self.mix_smooth.push(SmoothMix {
                vol: t.volume,
                pan: t.pan,
                dly: t.delay_send,
                rev: t.reverb_send,
            });
        }
        self.track_lat_delay.resize_with(nmix, RingDelay::default);
        self.direct_pdc_delay.resize_with(nmix, RingDelay::default);
        self.route_pdc_delay.resize_with(nmix, Vec::new);
        for (i, t) in song.mixer_tracks.iter().enumerate() {
            self.track_lat_delay[i].reserve(t.latency_samples);
            self.direct_pdc_delay[i].reserve(self.pdc_direct_delay.get(i).copied().unwrap_or(0));
            let rlen = t.routes.len();
            self.route_pdc_delay[i].resize_with(rlen, RingDelay::default);
            for (ri, rdelay) in self.route_pdc_delay[i].iter_mut().enumerate() {
                let d = self
                    .pdc_route_delay
                    .get(i)
                    .and_then(|v| v.get(ri))
                    .map(|x| x.1)
                    .unwrap_or(0);
                rdelay.reserve(d);
            }
        }
    }

    /// (Re)build voice pools and mixer PDC plan to match the song.
    pub fn ensure_channels(&mut self, song: &Song) {
        self.ensure_mixer_graph(song);
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
            let base_hz = midi_hz(ch.midi_base as f32);
            let mut pool = Vec::with_capacity(VOICES_PER_CH);
            for _ in 0..VOICES_PER_CH {
                let pitch = shared(base_hz);
                let gate = shared(0.0);
                let cutoff = shared(ch.cutoff);
                let unit = make_voice(spec, &pitch, &gate, &cutoff, self.sr as f64);
                pool.push(VoiceRt {
                    unit,
                    pitch,
                    gate,
                    cutoff,
                    vel: 1.0,
                    pan: 0.0,
                    cur_hz: base_hz,
                    target_hz: base_hz,
                    glide_rate: 1.0,
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
        self.channel_last_hz = vec![None; song.channels.len()];
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

    /// First free voice within polyphony cap, else steal the oldest-sounding one.
    fn alloc_voice(&mut self, ch: usize, max_poly: u8) -> Option<&mut VoiceRt> {
        let pool = self.voices.get_mut(ch)?;
        let cap = if max_poly == 0 {
            pool.len()
        } else {
            (max_poly as usize).clamp(1, pool.len())
        };
        let sub = &mut pool[..cap];
        let idx = sub
            .iter()
            .position(|v| v.off == 0)
            .or_else(|| {
                sub.iter()
                    .enumerate()
                    .min_by_key(|(_, v)| v.age)
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        self.age = self.age.wrapping_add(1);
        let v = &mut sub[idx];
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
        let hz = midi_hz(midi as f32);
        self.live_voice.pitch.set_value(hz);
        self.live_voice.cur_hz = hz;
        self.live_voice.target_hz = hz;
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
                let hz = midi_hz(midi as f32);
                let poly = channel.polyphony;
                if let Some(v) = self.alloc_voice(ch, poly) {
                    v.pitch.set_value(hz);
                    v.cur_hz = hz;
                    v.target_hz = hz;
                    v.glide_rate = 1.0;
                    v.gate.set_value(1.0);
                    v.vel = vel.clamp(0.05, 1.0);
                    v.pan = 0.0;
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
            let solo_tracks = song.playlist_tracks.iter().any(|t| t.solo);
            let bar_ticks = song.time_sig.ticks_per_bar().max(1) as u64;
            let beat_ticks = song.time_sig.ticks_per_beat().max(1) as u64;

            for k in k_start..k_end {
                let tick_at = (k as f64 * spt).round() as u64;
                if self.song_mode {
                    for clip in &song.clips {
                        if clip.mute {
                            continue;
                        }
                        if let Some(tr) = song.playlist_tracks.get(clip.track)
                            && ((solo_tracks && !tr.solo) || (!solo_tracks && tr.mute))
                        {
                            continue;
                        }
                        let start = clip.start_tick as u64;
                        let end = start + clip.len_ticks as u64;
                        match &clip.kind {
                            ClipKind::Pattern { pattern } if k >= start && k < end => {
                                let Some(pdata) = song.patterns.get(*pattern) else {
                                    continue;
                                };
                                let pat_len = pdata.len_ticks().max(1) as u64;
                                let in_pat =
                                    ((clip.offset_ticks as u64 + (k - start)) % pat_len) as u32;
                                self.cur_pat = *pattern;
                                self.cur_tick = in_pat;
                                self.collect_pattern_tick(song, pdata, in_pat, tick_at, spt);
                            }
                            ClipKind::Audio {
                                sample,
                                gain,
                                pitch_semitones,
                                fade_in_ticks,
                                fade_out_ticks,
                            } if k == start => {
                                let mixer_insert = song
                                    .playlist_tracks
                                    .get(clip.track)
                                    .and_then(|t| t.linked_insert)
                                    .unwrap_or(0);
                                if self.pending.len() < PENDING_CAP {
                                    self.pending.push((
                                        tick_at,
                                        SongEvent::AudioClip {
                                            sample: *sample,
                                            gain: *gain,
                                            pitch_semitones: *pitch_semitones,
                                            offset_ticks: clip.offset_ticks,
                                            len_ticks: clip.len_ticks,
                                            fade_in_ticks: *fade_in_ticks,
                                            fade_out_ticks: *fade_out_ticks,
                                            mixer_insert,
                                        },
                                    ));
                                }
                            }
                            ClipKind::Automation { target, points } if k >= start && k < end => {
                                if let Some(v) = eval_auto_clip(points, (k - start) as f32) {
                                    self.apply_auto_target(*target, v);
                                }
                            }
                            _ => {}
                        }
                    }
                    if song.metronome
                        && k.is_multiple_of(beat_ticks)
                        && self.pending.len() < PENDING_CAP
                    {
                        self.pending.push((
                            tick_at,
                            SongEvent::Click {
                                accent: k.is_multiple_of(bar_ticks),
                            },
                        ));
                    }
                } else {
                    let pat = self.pattern.min(song.patterns.len().saturating_sub(1));
                    let Some(pdata) = song.patterns.get(pat) else {
                        continue;
                    };
                    let pat_len = pdata.len_ticks().max(1) as u64;
                    let in_pat = (k % pat_len) as u32;
                    self.cur_pat = pat;
                    self.cur_tick = in_pat;
                    self.collect_pattern_tick(song, pdata, in_pat, tick_at, spt);
                    let pat_beat = pdata.time_sig.ticks_per_beat().max(1);
                    if song.metronome
                        && in_pat.is_multiple_of(pat_beat)
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
                        pan,
                        fine_pitch,
                        len_ticks,
                    } => {
                        let trig = NoteTrigger {
                            ch,
                            midi,
                            vel,
                            pan,
                            fine_pitch,
                            len_ticks,
                        };
                        self.fire_note(song, &trig, spt);
                    }
                    SongEvent::Click { accent } => {
                        if self.metro_heads.len() < 8 {
                            self.metro_heads.push(MetroHead { accent, pos: 0 });
                        }
                    }
                    SongEvent::AudioClip {
                        sample,
                        gain,
                        pitch_semitones,
                        offset_ticks,
                        len_ticks,
                        fade_in_ticks,
                        fade_out_ticks,
                        mixer_insert,
                    } => {
                        let s_sr = song
                            .samples
                            .get(sample)
                            .map(|s| s.sr.max(1.0))
                            .unwrap_or(self.sr.max(1.0));
                        let ratio = 2.0_f32.powf(pitch_semitones / 12.0);
                        let ticks_to_src = spt as f32 * (s_sr / self.sr.max(1.0)) * ratio;
                        let start_pos = offset_ticks as f32 * ticks_to_src;
                        let span_src = len_ticks.max(1) as f32 * ticks_to_src;
                        let mut head = PlayHead::new(
                            PlayKind::Sample(sample),
                            None,
                            ratio,
                            gain.clamp(0.0, 2.0),
                            usize::MAX,
                        );
                        head.mixer_insert = mixer_insert;
                        head.pos = start_pos;
                        head.end_pos = Some(start_pos + span_src);
                        head.fade_in_samples = (fade_in_ticks as f64 * spt) as usize;
                        head.fade_out_samples = (fade_out_ticks as f64 * spt) as usize;
                        self.push_playhead(head);
                    }
                }
                ei += 1;
            }
            // Envelopes & portamento always tick so live preview works while stopped.
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
            if self.song_mode {
                let total = song.playlist_len_ticks() as u64;
                if total > 0 {
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
    }

    fn apply_auto_target(&mut self, target: AutoTarget, value: f32) {
        match target {
            AutoTarget::ChannelVol(ci) => {
                if ci < self.auto_vol.len() {
                    self.auto_vol[ci] = value.clamp(0.0, 1.5);
                }
            }
            AutoTarget::ChannelCut(ci) => {
                let v = value.clamp(40.0, 18000.0);
                if ci < self.auto_cut.len() {
                    self.auto_cut[ci] = Some(v);
                }
                if ci < self.voices.len() {
                    for voice in self.voices[ci].iter() {
                        voice.cutoff.set_value(v);
                    }
                }
            }
            AutoTarget::InsertVol(ins) => {
                if ins < self.mix_auto_vol.len() {
                    self.mix_auto_vol[ins] = value.clamp(0.0, 1.5);
                }
            }
            AutoTarget::MasterVol => {
                self.master_auto_vol = Some(value.clamp(0.0, 1.5));
            }
        }
    }

    fn collect_pattern_tick(
        &mut self,
        song: &Song,
        pdata: &crate::song::Pattern,
        in_pat: u32,
        tick_at: u64,
        spt: f64,
    ) {
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
                                pan: note.pan,
                                fine_pitch: note.fine_pitch,
                                len_ticks: note.len.max(1),
                            },
                        ));
                    }
                }
            }
        }
    }

    fn tick_gates(&mut self) {
        for pool in self.voices.iter_mut() {
            for v in pool.iter_mut() {
                if (v.cur_hz - v.target_hz).abs() > 0.01 {
                    v.cur_hz += (v.target_hz - v.cur_hz) * v.glide_rate;
                    v.pitch.set_value(v.cur_hz);
                }
                if v.off > 0 {
                    v.off -= 1;
                    if v.off == 0 {
                        v.gate.set_value(0.0);
                    }
                }
            }
        }
    }

    fn fire_note(&mut self, song: &Song, trig: &NoteTrigger, spt: f64) {
        let NoteTrigger {
            ch,
            midi,
            vel,
            pan,
            fine_pitch,
            len_ticks,
        } = *trig;
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
        // Choke groups: cut any channel whose cut_by matches this channel's cut_group.
        if let Some(group) = channel.cut_group
            && group > 0
        {
            for (cj, target_ch) in song.channels.iter().enumerate() {
                if target_ch.cut_by == Some(group) {
                    if let Some(pool) = self.voices.get_mut(cj) {
                        for v in pool.iter_mut() {
                            v.gate.set_value(0.0);
                            v.off = 0;
                        }
                    }
                    for h in self.playheads.iter_mut() {
                        if h.ch == Some(cj) {
                            h.gate_off = h.env_sample;
                        }
                    }
                }
            }
        }

        let off = ((len_ticks as f64 * spt) as usize).max(1);
        let pitch_midi = (midi as f32 + fine_pitch).clamp(0.0, 127.0);
        let poly = channel.polyphony;
        let porta = channel.portamento;
        let base_midi = channel.midi_base;
        match channel.kind {
            ChannelKind::Synth { .. } => {
                if ch < self.voices.len() {
                    let target_hz = midi_hz(pitch_midi);
                    let prev_hz = self.channel_last_hz.get(ch).and_then(|x| *x);
                    let glide = if porta > 0.001 && prev_hz.is_some() {
                        (1.0 - (-1.0 / (porta * self.sr.max(1.0))).exp()).clamp(0.0005, 1.0)
                    } else {
                        1.0
                    };
                    let start_hz = if glide < 1.0 {
                        prev_hz.unwrap_or(target_hz)
                    } else {
                        target_hz
                    };
                    if let Some(v) = self.alloc_voice(ch, poly) {
                        v.cur_hz = start_hz;
                        v.target_hz = target_hz;
                        v.glide_rate = glide;
                        v.pitch.set_value(start_hz);
                        v.gate.set_value(1.0);
                        v.vel = vel.clamp(0.05, 1.0);
                        v.pan = pan.clamp(-1.0, 1.0);
                        v.off = off;
                    }
                    if ch < self.channel_last_hz.len() {
                        self.channel_last_hz[ch] = Some(target_hz);
                    }
                }
            }
            ChannelKind::Kit { piece } => {
                let ratio = 2.0_f32.powf(fine_pitch / 12.0);
                let mut head =
                    PlayHead::new(PlayKind::Kit(piece), Some(ch), ratio, vel, usize::MAX);
                head.pan = pan.clamp(-1.0, 1.0);
                self.push_playhead(head);
            }
            ChannelKind::Sampler { sample } => {
                if let Some(idx) = sample {
                    let ratio = 2.0_f32.powf((pitch_midi - base_midi as f32) / 12.0);
                    let mut head = PlayHead::new(PlayKind::Sample(idx), Some(ch), ratio, vel, off);
                    head.pan = pan.clamp(-1.0, 1.0);
                    self.push_playhead(head);
                }
            }
        }
    }

    fn render_sample(&mut self, song: &Song) -> (f32, f32) {
        let mut dly_send = 0.0f32;
        let mut rev_send = 0.0f32;

        for pair in self.ch_accum.iter_mut() {
            *pair = (0.0, 0.0);
        }
        for pair in self.mix_direct_in.iter_mut() {
            *pair = (0.0, 0.0);
        }
        for pair in self.mix_route_in.iter_mut() {
            *pair = (0.0, 0.0);
        }

        // Accumulate synth voices per channel (with per-note pan offset).
        for (ci, pool) in self.voices.iter_mut().enumerate() {
            let mut vl_sum = 0.0f32;
            let mut vr_sum = 0.0f32;
            for v in pool.iter_mut() {
                let (vl, vr) = v.unit.get_stereo();
                let (pl, pr) = pan_gains(v.pan, std::f32::consts::SQRT_2 * v.vel);
                vl_sum += vl * pl;
                vr_sum += vr * pr;
            }
            if let Some(acc) = self.ch_accum.get_mut(ci) {
                acc.0 += vl_sum;
                acc.1 += vr_sum;
            }
        }

        // Render PCM playheads (kit / pitched sampler / playlist audio clips).
        let sr = self.sr.max(1.0);
        let nmix = song.mixer_tracks.len();
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
                let past_end = h.end_pos.is_some_and(|e| h.pos >= e);
                if !sr_ok || (h.pos as usize) >= data.len() || past_end {
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
                            let (pl, pr) = pan_gains(h.pan, std::f32::consts::SQRT_2);
                            acc.0 += v * pl;
                            acc.1 += v * pr;
                        }
                    } else {
                        let mut fade = 1.0f32;
                        if h.fade_in_samples > 0 && h.env_sample < h.fade_in_samples {
                            fade *= h.env_sample as f32 / h.fade_in_samples as f32;
                        }
                        if h.fade_out_samples > 0
                            && let Some(end_p) = h.end_pos
                        {
                            let rem_samples =
                                (end_p - h.pos).max(0.0) / (base_ratio * h.pitch_ratio).max(1e-4);
                            if (rem_samples as usize) < h.fade_out_samples {
                                fade *= (rem_samples / h.fade_out_samples as f32).clamp(0.0, 1.0);
                            }
                        }
                        h.env_sample += 1;
                        let s_out = v * 0.9 * fade;
                        let target = h.mixer_insert.min(nmix.saturating_sub(1));
                        if let Some(m) = self.mix_direct_in.get_mut(target) {
                            m.0 += s_out;
                            m.1 += s_out;
                        }
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

        // Smooth channel controls, pan, feed mixer inserts, meter, and accumulate sends.
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
            let target = channel.mixer_insert.min(nmix.saturating_sub(1));
            if let Some(m) = self.mix_direct_in.get_mut(target) {
                m.0 += cl;
                m.1 += cr;
            }
            let mono = (cl + cr) * 0.5;
            dly_send += mono * dly_s;
            rev_send += mono * rev_s;
            if ci < self.ch_peaks.len() {
                let p = cl.abs().max(cr.abs());
                self.ch_peaks[ci] = (self.ch_peaks[ci] * 0.94).max(p);
            }
        }

        // Route live fallback synth + metronome into Master (insert 0).
        let (mut m_live_l, mut m_live_r) = self.live_voice.unit.get_stereo();
        m_live_l *= 0.8;
        m_live_r *= 0.8;
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
                    m_live_l += v;
                    m_live_r += v;
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

        // Process Mixer Insert DAG in topological order with automatic PDC.
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        let solo_inserts = song.mixer_tracks.iter().skip(1).any(|m| m.solo);
        for oi in 0..self.pdc_order.len() {
            let t = self.pdc_order[oi];
            let Some(track) = song.mixer_tracks.get(t) else {
                continue;
            };
            let d_del = self.pdc_direct_delay.get(t).copied().unwrap_or(0);
            let direct = self.direct_pdc_delay[t].push_pop(self.mix_direct_in[t], d_del);
            let pre = (
                direct.0 + self.mix_route_in[t].0,
                direct.1 + self.mix_route_in[t].1,
            );
            let post_lat = self.track_lat_delay[t].push_pop(pre, track.latency_samples);

            let (t_vol, t_pan, t_dly, t_rev) = if let Some(sm) = self.mix_smooth.get_mut(t) {
                sm.vol += (track.volume - sm.vol) * smooth_k;
                sm.pan += (track.pan - sm.pan) * smooth_k;
                sm.dly += (track.delay_send - sm.dly) * smooth_k;
                sm.rev += (track.reverb_send - sm.rev) * smooth_k;
                (sm.vol, sm.pan, sm.dly, sm.rev)
            } else {
                (track.volume, track.pan, track.delay_send, track.reverb_send)
            };
            let is_muted = if t == 0 {
                track.mute
            } else {
                (solo_inserts && !track.solo) || (!solo_inserts && track.mute)
            };
            let auto_g = self.mix_auto_vol.get(t).copied().unwrap_or(1.0);
            let gain = if is_muted { 0.0 } else { t_vol * auto_g };
            let (gl, gr) = pan_gains(t_pan, gain * std::f32::consts::SQRT_2);
            let tl = post_lat.0 * gl;
            let tr = post_lat.1 * gr;

            if t < self.mix_peaks.len() {
                self.mix_peaks[t] = (self.mix_peaks[t] * 0.94).max(tl.abs().max(tr.abs()));
            }
            let mono_t = (tl + tr) * 0.5;
            dly_send += mono_t * t_dly;
            rev_send += mono_t * t_rev;

            if t == 0 {
                l += tl;
                r += tr;
            } else {
                for (ri, route) in track.routes.iter().enumerate() {
                    let r_del = self
                        .pdc_route_delay
                        .get(t)
                        .and_then(|v| v.get(ri))
                        .map(|x| x.1)
                        .unwrap_or(0);
                    let routed = if let Some(rd) =
                        self.route_pdc_delay.get_mut(t).and_then(|v| v.get_mut(ri))
                    {
                        rd.push_pop((tl, tr), r_del)
                    } else {
                        (tl, tr)
                    };
                    if !route.sidechain_only && route.target < self.mix_route_in.len() {
                        self.mix_route_in[route.target].0 += routed.0 * route.amount;
                        self.mix_route_in[route.target].1 += routed.1 * route.amount;
                    }
                }
            }
        }

        // Sends: manual stereo delay + fundsp Schroeder reverb.
        let (dl, dr) = self
            .delay
            .process(self.sr, song.delay_time, dly_send, dly_send);
        let (rl, rr) = self.reverb.filter_stereo(rev_send, rev_send);
        l += dl * song.delay_mix + rl * song.reverb_mix + m_live_l;
        r += dr * song.delay_mix + rr * song.reverb_mix + m_live_r;

        let master_scale = song.master_vol * self.master_auto_vol.unwrap_or(1.0) * 0.22;
        l *= master_scale;
        r *= master_scale;
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
        self.master_auto_vol = None;
        self.mix_auto_vol.fill(1.0);
        for d in self.track_lat_delay.iter_mut() {
            d.clear();
        }
        for d in self.direct_pdc_delay.iter_mut() {
            d.clear();
        }
        for row in self.route_pdc_delay.iter_mut() {
            for d in row.iter_mut() {
                d.clear();
            }
        }
        for p in self.ch_peaks.iter_mut() {
            *p = 0.0;
        }
        for p in self.mix_peaks.iter_mut() {
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
    mix_peaks: [AtomicU32; MAX_TELEM_CH],
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
            mix_peaks: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
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
        for (i, p) in self.rt.mix_peaks.iter().take(MAX_TELEM_CH).enumerate() {
            t.mix_peaks[i].store(p.to_bits(), Ordering::Relaxed);
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
        let nmix = self.song.mixer_tracks.len().min(MAX_TELEM_CH);
        if self.rt.mix_peaks.len() != self.song.mixer_tracks.len() {
            self.rt.mix_peaks.resize(self.song.mixer_tracks.len(), 0.0);
        }
        for i in 0..nmix {
            self.rt.mix_peaks[i] = f32::from_bits(t.mix_peaks[i].load(Ordering::Relaxed));
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
        song.samples.push(crate::song::SampleRef {
            name: "tone.wav".into(),
            path: "tone.wav".into(),
            data: vec![0.5; 4410].into(),
            sr: 44100.0,
        });
        song.add_audio_clip(0, 0, 0, 1.0);
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

    #[test]
    fn pdc_aligns_parallel_mixer_paths() {
        let mut song = crate::song::new_empty();
        let mut imp = vec![0.0f32; 512];
        // Place the impulse after the 44-sample (1 ms) attack ramp reaches full gain.
        for s in &mut imp[64..80] {
            *s = 1.0;
        }
        song.samples.push(crate::song::SampleRef {
            name: "imp.wav".into(),
            path: "imp.wav".into(),
            data: imp.into(),
            sr: 44100.0,
        });
        // Two channels triggering identical pulse at tick 0 -> Insert 1 (lat=64) and Insert 2 (lat=0).
        song.channels[0].kind = ChannelKind::Sampler { sample: Some(0) };
        song.channels[0].mixer_insert = 1;
        song.channels[0].cutoff = 18000.0;
        song.channels[0].attack = 0.001;
        song.channels[0].reverb_send = 0.0;
        song.channels[1].kind = ChannelKind::Sampler { sample: Some(0) };
        song.channels[1].mixer_insert = 2;
        song.channels[1].cutoff = 18000.0;
        song.channels[1].attack = 0.001;
        song.channels[1].reverb_send = 0.0;
        song.mixer_tracks[1].latency_samples = 64;
        song.mixer_tracks[2].latency_samples = 0;
        song.patterns[0].notes[0].push(crate::song::Note::new(0, 8, 60, 1.0));
        song.patterns[0].notes[1].push(crate::song::Note::new(0, 8, 60, 1.0));

        let mut rt = EngineRt::new(44100.0);
        rt.playing = true;
        let mut l = vec![0.0; 512];
        let mut r = vec![0.0; 512];
        rt.process(&song, &mut l, &mut r);

        // Pulse starts at sample 64, and both Insert 1 (+64 latency) and Insert 2 (+64 PDC)
        // delay it to sample 128; if Insert 2 were uncompensated it would appear at 64..128.
        let early_peak = l[..128].iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        let late_peak = l[128..200].iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        assert!(early_peak < 1e-4, "uncompensated early audio: {early_peak}");
        assert!(late_peak > 0.01, "aligned signal missing: {late_peak}");
    }

    #[test]
    fn choke_group_cuts_target_channel() {
        let mut song = crate::song::new_empty();
        song.channels[4].cut_by = Some(1);
        song.channels[0].cut_group = Some(1);
        song.patterns[0].notes[4].push(crate::song::Note::new(0, 32, 48, 1.0));
        song.patterns[0].notes[0].push(crate::song::Note::new(4, 4, 36, 1.0));
        let mut rt = EngineRt::new(44100.0);
        rt.playing = true;
        // At 128 BPM (~1292 samples/tick), tick 4 fires around sample 5168.
        let mut l = vec![0.0; 8192];
        let mut r = vec![0.0; 8192];
        rt.process(&song, &mut l, &mut r);
        assert!(
            rt.voices[4].iter().all(|v| v.off == 0),
            "choke group must cut ringing synth voices"
        );
    }

    #[test]
    fn multi_track_overlapping_clips_and_auto_render() {
        let mut song = crate::song::new_empty();
        song.patterns[0].notes[0].push(crate::song::Note::new(0, 4, 36, 1.0));
        song.patterns[1].notes[4].push(crate::song::Note::new(0, 16, 45, 1.0));
        song.add_pattern_clip(0, 0, 0);
        song.add_pattern_clip(1, 0, 1);
        song.add_auto_clip(2, 0, 64, AutoTarget::MasterVol);
        let mut rt = EngineRt::new(44100.0);
        rt.song_mode = true;
        rt.playing = true;
        let mut l = vec![0.0; 8192];
        let mut r = vec![0.0; 8192];
        rt.process(&song, &mut l, &mut r);
        assert!(
            rt.ch_peaks[0] > 0.01 && rt.ch_peaks[4] > 0.01,
            "ch0={} ch4={}",
            rt.ch_peaks[0],
            rt.ch_peaks[4]
        );
        assert!(
            rt.mix_peaks[1] > 0.01 && rt.mix_peaks[5] > 0.01,
            "mix1={} mix5={}",
            rt.mix_peaks[1],
            rt.mix_peaks[5]
        );
    }
}
