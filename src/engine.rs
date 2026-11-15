//! Realtime engine: lock-free cpal backend, fundsp synth voices (polyphonic pools),
//! PCM kit + pitched sampler playback routed through per-channel DSP,
//! delay/reverb sends, sample-clock scheduler, and disk-streaming take recorder.

use crate::fx::{FFT_BINS, MixerInsertRt, compute_fft_bins};
use crate::midi::MidiMsg;
use crate::song::{
    AutoTarget, ChannelKind, ClipKind, KitPiece, MAX_FX_SLOTS, SampleRef, Song, StretchMode,
    VoiceSpec, WaveKind, eval_auto, eval_auto_clip,
};
use crate::synth::AdvVoiceRt;
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
    slide_rem: usize,
    slide_tot: usize,
    slide_start_hz: f32,
    slide_vel_start: f32,
    slide_vel_target: f32,
    slide_pan_start: f32,
    slide_pan_target: f32,
    off: usize,
    live_midi: Option<u8>,
    age: u64,
}

impl VoiceRt {
    fn new(spec: VoiceSpec, base_hz: f32, cutoff_hz: f32, sr: f64) -> Self {
        let pitch = shared(base_hz);
        let gate = shared(0.0);
        let cutoff = shared(cutoff_hz);
        let unit = make_voice(spec, &pitch, &gate, &cutoff, sr);
        Self {
            unit,
            pitch,
            gate,
            cutoff,
            vel: 1.0,
            pan: 0.0,
            cur_hz: base_hz,
            target_hz: base_hz,
            glide_rate: 1.0,
            slide_rem: 0,
            slide_tot: 0,
            slide_start_hz: base_hz,
            slide_vel_start: 1.0,
            slide_vel_target: 1.0,
            slide_pan_start: 0.0,
            slide_pan_target: 0.0,
            off: 0,
            live_midi: None,
            age: 0,
        }
    }
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

/// Load an audio file (wav/mp3/ogg/...) into full stereo (L, optional R) f32 + native sample rate.
pub fn load_sample_file(path: &str) -> Result<(Vec<f32>, Vec<f32>, f32), String> {
    let wave = Wave::load(path).map_err(|e| format!("{e}"))?;
    if wave.is_empty() {
        return Err("empty audio file".into());
    }
    let n = wave.len();
    let nch = wave.channels().max(1);
    let left: Vec<f32> = (0..n)
        .map(|i| wave.channel(0).get(i).copied().unwrap_or(0.0))
        .collect();
    let right: Vec<f32> = if nch >= 2 {
        (0..n)
            .map(|i| wave.channel(1).get(i).copied().unwrap_or(0.0))
            .collect()
    } else {
        Vec::new()
    };
    Ok((left, right, wave.sample_rate() as f32))
}

#[inline]
fn cubic_hermite(y_m1: f32, y0: f32, y1: f32, y2: f32, t: f32) -> f32 {
    let c0 = y0;
    let c1 = 0.5 * (y1 - y_m1);
    let c2 = y_m1 - 2.5 * y0 + 2.0 * y1 - 0.5 * y2;
    let c3 = 0.5 * (y2 - y_m1) + 1.5 * (y0 - y1);
    ((c3 * t + c2) * t + c1) * t + c0
}

fn read_sample_hermite_lr(smp: &SampleRef, pos: f32) -> (f32, f32) {
    let len = smp.data.len();
    if len == 0 || pos < 0.0 || pos >= len as f32 {
        return (0.0, 0.0);
    }
    let p0 = pos as usize;
    let frac = pos - p0 as f32;
    let i_m1 = p0.saturating_sub(1);
    let i1 = (p0 + 1).min(len - 1);
    let i2 = (p0 + 2).min(len - 1);
    let l = cubic_hermite(
        smp.data[i_m1],
        smp.data[p0],
        smp.data[i1],
        smp.data[i2],
        frac,
    );
    let r = if smp.is_stereo() {
        cubic_hermite(
            smp.data_r[i_m1],
            smp.data_r[p0],
            smp.data_r[i1],
            smp.data_r[i2],
            frac,
        )
    } else {
        l
    };
    (l, r)
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
    direct_insert: Option<usize>,
    pad_cut_by: Option<u8>,
    mixer_insert: usize,
    pos: f32,
    start_pos: f32,
    end_pos: Option<f32>,
    loop_enabled: bool,
    loop_start: f32,
    loop_end: f32,
    loop_xfade: f32,
    reverse: bool,
    granular: bool,
    grain_phase: f32,
    grain_anchor: f32,
    filter_env_amt: f32,
    pitch_ratio: f32,
    slide_start_ratio: f32,
    slide_target_ratio: f32,
    slide_rem: usize,
    slide_tot: usize,
    gain: f32,
    pan: f32,
    cutoff_mul: f32,
    rel_mul: f32,
    fade_in_samples: usize,
    fade_out_samples: usize,
    lp_l: f32,
    lp_r: f32,
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
            direct_insert: None,
            pad_cut_by: None,
            mixer_insert: 0,
            pos: 0.0,
            start_pos: 0.0,
            end_pos: None,
            loop_enabled: false,
            loop_start: 0.0,
            loop_end: 0.0,
            loop_xfade: 0.05,
            reverse: false,
            granular: false,
            grain_phase: 0.0,
            grain_anchor: 0.0,
            filter_env_amt: 0.0,
            pitch_ratio,
            slide_start_ratio: pitch_ratio,
            slide_target_ratio: pitch_ratio,
            slide_rem: 0,
            slide_tot: 0,
            gain,
            pan: 0.0,
            cutoff_mul: 1.0,
            rel_mul: 1.0,
            fade_in_samples: 0,
            fade_out_samples: 0,
            lp_l: 0.0,
            lp_r: 0.0,
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
        let r_n = (r.max(0.005) * self.rel_mul * sr) as usize;
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

#[derive(Clone, Copy)]
struct NoteTrigger {
    ch: usize,
    midi: u8,
    vel: f32,
    pan: f32,
    fine_pitch: f32,
    cutoff: f32,
    release_bias: f32,
    len_ticks: u32,
}

enum SongEvent {
    NoteOn(NoteTrigger),
    Slide(NoteTrigger),
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
        h ^= ((t.effective_latency_samples() as u64) << 16) ^ (i as u64);
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
    adv_voices: Vec<Vec<AdvVoiceRt>>,
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
    pub mix_rms: Vec<f32>,
    pub mix_clips: Vec<bool>,
    pub inspect_insert: usize,
    pub fft_bins: [f32; FFT_BINS],
    pub comp_gr_db: [f32; MAX_FX_SLOTS],
    fft_tick: usize,
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
    auto_pan: Vec<f32>,
    auto_cut: Vec<Option<f32>>,
    mix_auto_vol: Vec<f32>,
    mix_auto_pan: Vec<f32>,
    master_auto_vol: Option<f32>,
    peak_ctrl_env: Vec<f32>,
    peak_ctrl_lfo: Vec<f32>,
    smooth: Vec<SmoothCh>,
    mix_smooth: Vec<SmoothMix>,
    mix_fx: Vec<MixerInsertRt>,
    mix_sc_bus: Vec<(f32, f32)>,
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
    pub midi_target_ch: usize,
    pub midi_sustain: bool,
    pub midi_bend_semi: f32,
    pub midi_mod_wheel: f32,
    midi_sustain_pending: [bool; 128],
    pub input_monitor: bool,
    pub input_insert: usize,
    pub input_rec_only: bool,
    pub input_peak: f32,
    pub input_rx: Option<rtrb::Consumer<f32>>,
    last_input: (f32, f32),
    pub take_in_l: Vec<f32>,
    pub take_in_r: Vec<f32>,
    pub count_in_rem_samples: usize,
    count_in_tick_samples: usize,
}

#[derive(Clone, Copy)]
struct HeadSpawn {
    ch: usize,
    pitch_midi: f32,
    vel: f32,
    pan: f32,
    cutoff_mul: f32,
    rel_mul: f32,
    off: usize,
}

impl EngineRt {
    pub fn new(sr: f32) -> Self {
        let live_voice = VoiceRt::new(LIVE_VOICE_SPEC, midi_hz(69.0_f32), 2600.0, sr as f64);
        let mut reverb: Box<dyn AudioUnit + Send> = Box::new(reverb_stereo(10.0, 2.5, 0.5));
        reverb.set_sample_rate(sr as f64);
        reverb.allocate();
        let mut rt = Self {
            sr,
            voices: Vec::new(),
            adv_voices: Vec::new(),
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
            mix_rms: Vec::new(),
            mix_clips: Vec::new(),
            inspect_insert: 0,
            fft_bins: [-72.0; FFT_BINS],
            comp_gr_db: [0.0; MAX_FX_SLOTS],
            fft_tick: 0,
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
            auto_pan: Vec::new(),
            auto_cut: Vec::new(),
            mix_auto_vol: Vec::new(),
            mix_auto_pan: Vec::new(),
            master_auto_vol: None,
            peak_ctrl_env: Vec::new(),
            peak_ctrl_lfo: Vec::new(),
            smooth: Vec::new(),
            mix_smooth: Vec::new(),
            mix_fx: Vec::new(),
            mix_sc_bus: Vec::new(),
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
            midi_target_ch: 0,
            midi_sustain: false,
            midi_bend_semi: 0.0,
            midi_mod_wheel: 0.0,
            midi_sustain_pending: [false; 128],
            input_monitor: false,
            input_insert: 1,
            input_rec_only: false,
            input_peak: 0.0,
            input_rx: None,
            last_input: (0.0, 0.0),
            take_in_l: Vec::new(),
            take_in_r: Vec::new(),
            count_in_rem_samples: 0,
            count_in_tick_samples: 0,
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
        self.adv_voices.clear();
        self.mix_fx.clear();
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
        self.mix_rms.resize(nmix, 0.0);
        self.mix_clips.resize(nmix, false);
        self.mix_auto_vol.resize(nmix, 1.0);
        self.mix_auto_pan.resize(nmix, 0.0);
        self.mix_direct_in.resize(nmix, (0.0, 0.0));
        self.mix_route_in.resize(nmix, (0.0, 0.0));
        self.mix_sc_bus.resize(nmix, (0.0, 0.0));
        while self.mix_fx.len() < nmix {
            self.mix_fx.push(MixerInsertRt::new(self.sr));
        }
        self.mix_fx.truncate(nmix);
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
            self.track_lat_delay[i].reserve(t.effective_latency_samples());
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
        if self.peak_ctrl_env.len() != song.peak_controllers.len() {
            self.peak_ctrl_env.resize(song.peak_controllers.len(), 0.0);
            self.peak_ctrl_lfo.resize(song.peak_controllers.len(), 0.0);
        }
        if self.voices.len() == song.channels.len() && self.adv_voices.len() == song.channels.len()
        {
            for i in 0..song.channels.len() {
                let spec = song.channels[i].voice_spec();
                if self.voice_specs.get(i) != Some(&spec) {
                    self.rebuild_channel(song, i);
                }
            }
            return;
        }
        self.voices.clear();
        self.adv_voices.clear();
        self.smooth.clear();
        self.voice_specs.clear();
        for ch in &song.channels {
            let spec = ch.voice_spec();
            let base_hz = midi_hz(ch.midi_base as f32);
            let mut pool = Vec::with_capacity(VOICES_PER_CH);
            for _ in 0..VOICES_PER_CH {
                pool.push(VoiceRt::new(spec, base_hz, ch.cutoff, self.sr as f64));
            }
            self.voices.push(pool);
            self.adv_voices
                .push(vec![AdvVoiceRt::default(); VOICES_PER_CH]);
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
        self.auto_pan = vec![0.0; song.channels.len()];
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
        if idx >= self.voices.len() || idx >= self.adv_voices.len() {
            self.ensure_channels(song);
            return;
        }
        let spec = song.channels[idx].voice_spec();
        let cutoff_val = song.channels[idx].cutoff;
        for v in self.voices[idx].iter_mut() {
            v.unit = make_voice(spec, &v.pitch, &v.gate, &v.cutoff, self.sr as f64);
            v.cutoff.set_value(cutoff_val);
            v.slide_rem = 0;
            v.off = 0;
            v.live_midi = None;
        }
        for v in self.adv_voices[idx].iter_mut() {
            v.kill();
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

    fn alloc_adv_voice(&mut self, ch: usize, max_poly: u8) -> Option<&mut AdvVoiceRt> {
        let pool = self.adv_voices.get_mut(ch)?;
        let cap = if max_poly == 0 {
            pool.len()
        } else {
            (max_poly as usize).clamp(1, pool.len())
        };
        let sub = &mut pool[..cap];
        let idx = sub
            .iter()
            .position(|v| !v.active)
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

    fn spawn_sampler_head(&mut self, song: &Song, sample_idx: usize, h: HeadSpawn) {
        let Some(channel) = song.channels.get(h.ch) else {
            return;
        };
        let smp_len = song
            .samples
            .get(sample_idx)
            .map(|s| s.data.len())
            .unwrap_or(0) as f32;
        if smp_len <= 0.0 {
            return;
        }
        let cfg = channel.sampler;
        let t_start = (cfg.trim_start.clamp(0.0, 0.98) * smp_len).min(smp_len - 1.0);
        let t_end = (cfg.trim_end.clamp(0.02, 1.0) * smp_len).max(t_start + 1.0);
        let trim_span = (t_end - t_start).max(1.0);
        let l_start = t_start + cfg.loop_start.clamp(0.0, 0.95) * trim_span;
        let l_end = (t_start + cfg.loop_end.clamp(0.05, 1.0) * trim_span).max(l_start + 8.0);
        let ratio = 2.0_f32.powf((h.pitch_midi - channel.midi_base as f32) / 12.0);
        let mut head = PlayHead::new(
            PlayKind::Sample(sample_idx),
            Some(h.ch),
            ratio,
            h.vel,
            h.off,
        );
        head.pan = h.pan.clamp(-1.0, 1.0);
        head.cutoff_mul = h.cutoff_mul;
        head.rel_mul = h.rel_mul;
        head.start_pos = t_start;
        head.end_pos = Some(t_end);
        head.pos = if cfg.reverse {
            (t_end - 1.0).max(t_start)
        } else {
            t_start
        };
        head.loop_enabled = cfg.loop_enabled;
        head.loop_start = l_start;
        head.loop_end = l_end.min(t_end);
        head.loop_xfade = cfg.loop_xfade.clamp(0.0, 0.5);
        head.reverse = cfg.reverse;
        head.granular = cfg.stretch == StretchMode::Granular;
        head.grain_anchor = head.pos;
        head.filter_env_amt = cfg.filter_env_amt;
        self.push_playhead(head);
    }

    fn spawn_drum_pad_head(&mut self, song: &Song, midi: u8, h: HeadSpawn) {
        let Some(channel) = song.channels.get(h.ch) else {
            return;
        };
        let ChannelKind::DrumRack { pads } = &channel.kind else {
            return;
        };
        if pads.is_empty() {
            return;
        }
        let pad = pads
            .iter()
            .find(|p| p.note == midi)
            .unwrap_or(&pads[(midi as usize) % pads.len()]);

        // Pad-local choke group cuts other ringing pads on this DrumRack channel.
        if let Some(cg) = pad.cut_group
            && cg > 0
        {
            for head in self.playheads.iter_mut() {
                if head.ch == Some(h.ch) && head.pad_cut_by == Some(cg) {
                    head.gate_off = head.env_sample;
                }
            }
        }

        let layer = pad
            .layers
            .iter()
            .find(|l| h.vel >= l.vel_min && h.vel <= l.vel_max)
            .or_else(|| pad.layers.first());
        let Some(layer) = layer else {
            return;
        };
        let kind = if let Some(s_idx) = layer.sample {
            PlayKind::Sample(s_idx)
        } else if let Some(kit) = layer.kit_fallback {
            PlayKind::Kit(kit)
        } else {
            return;
        };

        let ratio = 2.0_f32.powf(pad.tune_semi / 12.0);
        let nmix = song.mixer_tracks.len().max(1);
        let ins = (channel.mixer_insert as isize + pad.mixer_offset as isize)
            .clamp(0, (nmix - 1) as isize) as usize;
        let mut head = PlayHead::new(
            kind,
            Some(h.ch),
            ratio,
            (h.vel * pad.gain).clamp(0.0, 2.0),
            usize::MAX,
        );
        head.pan = (h.pan + pad.pan).clamp(-1.0, 1.0);
        head.cutoff_mul = h.cutoff_mul;
        head.rel_mul = h.rel_mul;
        head.pad_cut_by = pad.cut_by;
        head.direct_insert = (pad.mixer_offset != 0).then_some(ins);
        self.push_playhead(head);
    }

    pub fn live_note(&mut self, midi: u8) {
        let hz = midi_hz(midi as f32);
        self.live_voice.pitch.set_value(hz);
        self.live_voice.cur_hz = hz;
        self.live_voice.target_hz = hz;
        self.live_voice.slide_rem = 0;
        self.live_voice.gate.set_value(1.0);
        self.live_off = (self.sr * 0.4) as usize;
    }

    /// Preview a note using the selected channel's real synth, kit, drum rack, or pitched sampler.
    pub fn live_note_ch(&mut self, song: &Song, ch: usize, midi: u8, vel: f32) {
        let off = (self.sr * 0.35) as usize;
        self.trigger_live_voice(song, ch, midi, vel, off, None);
    }

    fn trigger_live_voice(
        &mut self,
        song: &Song,
        ch: usize,
        midi: u8,
        vel: f32,
        off: usize,
        midi_tag: Option<u8>,
    ) {
        self.ensure_channels(song);
        let Some(channel) = song.channels.get(ch) else {
            self.live_note(midi);
            return;
        };
        let cut_val = (channel.cutoff * (1.0 + self.midi_mod_wheel * 1.5)).clamp(40.0, 18000.0);
        match &channel.kind {
            ChannelKind::Synth { .. } => {
                let hz = midi_hz(midi as f32);
                let poly = channel.polyphony;
                if let Some(v) = self.alloc_voice(ch, poly) {
                    v.pitch.set_value(hz);
                    v.cutoff.set_value(cut_val);
                    v.cur_hz = hz;
                    v.target_hz = hz;
                    v.glide_rate = 1.0;
                    v.slide_rem = 0;
                    v.gate.set_value(1.0);
                    v.vel = vel.clamp(0.05, 1.0);
                    v.pan = 0.0;
                    v.off = off;
                    v.live_midi = midi_tag;
                }
            }
            ChannelKind::SuperSynth(_) | ChannelKind::FmSynth(_) => {
                let hz = midi_hz(midi as f32);
                let poly = channel.polyphony;
                let bend = self.midi_bend_semi;
                let mw = self.midi_mod_wheel;
                if let Some(v) = self.alloc_adv_voice(ch, poly) {
                    v.trigger(hz, hz, 1.0, vel, 0.0, off);
                    v.live_midi = midi_tag;
                    v.bend_semi = bend;
                    v.cutoff_mul = 1.0 + mw * 1.5;
                }
            }
            ChannelKind::Kit { piece } => {
                self.push_playhead(PlayHead::new(
                    PlayKind::Kit(*piece),
                    Some(ch),
                    1.0,
                    vel.clamp(0.05, 1.0),
                    usize::MAX,
                ));
            }
            ChannelKind::DrumRack { .. } => {
                self.spawn_drum_pad_head(
                    song,
                    midi,
                    HeadSpawn {
                        ch,
                        pitch_midi: midi as f32,
                        vel: vel.clamp(0.05, 1.0),
                        pan: 0.0,
                        cutoff_mul: 1.0,
                        rel_mul: 1.0,
                        off,
                    },
                );
            }
            ChannelKind::Sampler { sample } => {
                if let Some(idx) = *sample {
                    self.spawn_sampler_head(
                        song,
                        idx,
                        HeadSpawn {
                            ch,
                            pitch_midi: midi as f32,
                            vel: vel.clamp(0.05, 1.0),
                            pan: 0.0,
                            cutoff_mul: 1.0,
                            rel_mul: 1.0,
                            off,
                        },
                    );
                }
            }
        }
    }

    fn release_live_midi_note(&mut self, midi: u8) {
        for pool in self.voices.iter_mut() {
            for v in pool.iter_mut() {
                if v.live_midi == Some(midi) {
                    v.gate.set_value(0.0);
                    v.off = 0;
                    v.live_midi = None;
                }
            }
        }
        for pool in self.adv_voices.iter_mut() {
            for v in pool.iter_mut() {
                if v.live_midi == Some(midi) {
                    v.release_note();
                }
            }
        }
    }

    pub fn handle_midi_msg(&mut self, song: &Song, msg: MidiMsg) {
        self.ensure_channels(song);
        let ch = self
            .midi_target_ch
            .min(song.channels.len().saturating_sub(1));
        match msg {
            MidiMsg::NoteOn { note, vel, .. } => {
                let idx = (note & 0x7F) as usize;
                self.midi_sustain_pending[idx] = false;
                self.trigger_live_voice(song, ch, note, vel, usize::MAX, Some(note));
            }
            MidiMsg::NoteOff { note, .. } => {
                let idx = (note & 0x7F) as usize;
                if self.midi_sustain {
                    self.midi_sustain_pending[idx] = true;
                } else {
                    self.release_live_midi_note(note);
                }
            }
            MidiMsg::Cc { cc: 64, val, .. } => {
                let down = val >= 64;
                if self.midi_sustain && !down {
                    for n in 0..128 {
                        if self.midi_sustain_pending[n] {
                            self.midi_sustain_pending[n] = false;
                            self.release_live_midi_note(n as u8);
                        }
                    }
                }
                self.midi_sustain = down;
            }
            MidiMsg::Cc { cc: 1, val, .. } => {
                self.midi_mod_wheel = val as f32 / 127.0;
                if let Some(channel) = song.channels.get(ch) {
                    let cut =
                        (channel.cutoff * (1.0 + self.midi_mod_wheel * 1.5)).clamp(40.0, 18000.0);
                    self.set_channel_cutoff(ch, cut);
                }
            }
            MidiMsg::Cc { .. } => {}
            MidiMsg::PitchBend { norm, .. } => {
                self.midi_bend_semi = norm.clamp(-1.0, 1.0) * 2.0;
                for pool in self.adv_voices.iter_mut() {
                    for v in pool.iter_mut() {
                        v.bend_semi = self.midi_bend_semi;
                    }
                }
            }
        }
    }

    pub fn arm_count_in(&mut self, song: &Song) {
        if song.count_in_bars == 0 {
            self.count_in_rem_samples = 0;
            self.count_in_tick_samples = 0;
            return;
        }
        let spt = (self.sr * 60.0 / song.bpm.max(30.0) / 16.0).max(1.0) as f64;
        let beat_samples = (song.time_sig.ticks_per_beat().max(1) as f64 * spt).round() as usize;
        let bar_samples = (song.time_sig.ticks_per_bar().max(1) as f64 * spt).round() as usize;
        self.count_in_rem_samples = (song.count_in_bars as usize).saturating_mul(bar_samples);
        self.count_in_tick_samples = beat_samples.max(1);
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

        // Pre-roll count-in metronome before starting sequencer clock or audio take capture.
        if self.playing && self.count_in_rem_samples > 0 {
            let beat_s = self.count_in_tick_samples.max(1);
            let bar_s =
                ((song.time_sig.ticks_per_bar().max(1) as f64 * spt).round() as usize).max(beat_s);
            for i in 0..n {
                if self.count_in_rem_samples > 0 {
                    if self.count_in_rem_samples.is_multiple_of(beat_s)
                        && self.metro_heads.len() < 8
                    {
                        self.metro_heads.push(MetroHead {
                            accent: self.count_in_rem_samples.is_multiple_of(bar_s),
                            pos: 0,
                        });
                    }
                    self.count_in_rem_samples -= 1;
                }
                self.tick_gates();
                let (l, r) = self.render_sample(song);
                out_l[i] = l;
                out_r[i] = r;
            }
            return;
        }

        let mut auto_indices = [0usize; 64];
        let mut n_auto = 0usize;

        // Collect this block's events on the integer sample clock (no float drift).
        if !self.playing {
            // Stopping drops notes that never started; ringing voices decay.
            self.pending.clear();
        }
        if self.playing {
            let k_start = (self.sample_pos as f64 / spt).ceil() as u64;
            let k_end = ((self.sample_pos + n as u64) as f64 / spt).ceil() as u64;
            let block_tick_lo = (self.sample_pos as f64 / spt).floor() as u64;
            let block_tick_hi = ((self.sample_pos + n as u64) as f64 / spt).ceil() as u64;
            let solo_tracks = song.playlist_tracks.iter().any(|t| t.solo);
            let bar_ticks = song.time_sig.ticks_per_bar().max(1) as u64;
            let beat_ticks = song.time_sig.ticks_per_beat().max(1) as u64;

            if self.song_mode {
                for (ci, clip) in song.clips.iter().enumerate() {
                    if clip.mute || !matches!(clip.kind, ClipKind::Automation { .. }) {
                        continue;
                    }
                    if let Some(tr) = song.playlist_tracks.get(clip.track)
                        && ((solo_tracks && !tr.solo) || (!solo_tracks && tr.mute))
                    {
                        continue;
                    }
                    let start = clip.start_tick as u64;
                    let end = start + clip.len_ticks as u64;
                    if start <= block_tick_hi && end >= block_tick_lo && n_auto < auto_indices.len()
                    {
                        auto_indices[n_auto] = ci;
                        n_auto += 1;
                    }
                }
            }

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
            if self.playing {
                self.tick_pos = now as f64 / spt;
                if self.song_mode && n_auto > 0 {
                    let frac_tick = self.tick_pos;
                    for &ci in &auto_indices[..n_auto] {
                        if let Some(clip) = song.clips.get(ci)
                            && let ClipKind::Automation { target, points } = &clip.kind
                        {
                            let s = clip.start_tick as f64;
                            let e = s + clip.len_ticks as f64;
                            if frac_tick >= s
                                && frac_tick < e
                                && let Some(v) = eval_auto_clip(points, (frac_tick - s) as f32)
                            {
                                self.apply_auto_target(*target, v);
                            }
                        }
                    }
                }
            }
            while ei < self.pending.len() && self.pending[ei].0 <= now {
                match self.pending[ei].1 {
                    SongEvent::NoteOn(trig) => {
                        self.fire_note(song, &trig, spt);
                    }
                    SongEvent::Slide(trig) => {
                        self.fire_slide(song, &trig, spt);
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
            AutoTarget::ChannelPan(ci) => {
                if ci < self.auto_pan.len() {
                    self.auto_pan[ci] = value.clamp(-1.0, 1.0);
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
            AutoTarget::InsertPan(ins) => {
                if ins < self.mix_auto_pan.len() {
                    self.mix_auto_pan[ins] = value.clamp(-1.0, 1.0);
                }
            }
            AutoTarget::InsertFxMix(ins, slot) => {
                if let Some(m) = self.mix_fx.get_mut(ins)
                    && slot < MAX_FX_SLOTS
                {
                    m.mix_override[slot] = Some(value.clamp(0.0, 1.0));
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
                for note in notes.iter().filter(|nn| nn.tick == in_pat && !nn.mute) {
                    let mut at = tick_at;
                    if in_pat % 8 == 4 {
                        at += (song.swing.clamp(0.0, 0.75) as f64 * 2.0 * spt) as u64;
                    }
                    if self.pending.len() < PENDING_CAP {
                        let trig = NoteTrigger {
                            ch: ci,
                            midi: note.midi,
                            vel: note.vel,
                            pan: note.pan,
                            fine_pitch: note.fine_pitch,
                            cutoff: note.cutoff,
                            release_bias: note.release_bias,
                            len_ticks: note.len.max(1),
                        };
                        let ev = if note.slide {
                            SongEvent::Slide(trig)
                        } else {
                            SongEvent::NoteOn(trig)
                        };
                        self.pending.push((at, ev));
                    }
                }
            }
        }
    }

    fn tick_gates(&mut self) {
        let bend_mul = 2.0_f32.powf(self.midi_bend_semi / 12.0);
        for pool in self.voices.iter_mut() {
            for v in pool.iter_mut() {
                if v.slide_rem > 0 && v.slide_tot > 0 {
                    v.slide_rem -= 1;
                    let frac = 1.0 - (v.slide_rem as f32 / v.slide_tot as f32);
                    let start = v.slide_start_hz.max(20.0);
                    let hz = (start * (v.target_hz / start).powf(frac)).clamp(20.0, 20000.0);
                    v.cur_hz = hz;
                    v.pitch.set_value((hz * bend_mul).clamp(20.0, 20000.0));
                    v.vel = v.slide_vel_start + (v.slide_vel_target - v.slide_vel_start) * frac;
                    v.pan = v.slide_pan_start + (v.slide_pan_target - v.slide_pan_start) * frac;
                } else if (v.cur_hz - v.target_hz).abs() > 0.01 {
                    v.cur_hz += (v.target_hz - v.cur_hz) * v.glide_rate;
                    v.pitch
                        .set_value((v.cur_hz * bend_mul).clamp(20.0, 20000.0));
                } else if v.off > 0 && self.midi_bend_semi.abs() > 1e-4 {
                    v.pitch
                        .set_value((v.cur_hz * bend_mul).clamp(20.0, 20000.0));
                }
                if v.off > 0 && v.off != usize::MAX {
                    v.off -= 1;
                    if v.off == 0 {
                        v.gate.set_value(0.0);
                        v.live_midi = None;
                    }
                }
            }
        }
        for pool in self.adv_voices.iter_mut() {
            for v in pool.iter_mut() {
                v.tick_gate();
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
            cutoff,
            release_bias,
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
                            v.slide_rem = 0;
                            v.off = 0;
                        }
                    }
                    if let Some(pool) = self.adv_voices.get_mut(cj) {
                        for v in pool.iter_mut() {
                            v.kill();
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
        let cutoff_mul = 2.0_f32.powf((cutoff.clamp(0.0, 1.0) - 0.5) * 4.0);
        let rel_mul = 2.0_f32.powf((release_bias.clamp(0.0, 1.0) - 0.5) * 2.0);
        let base_cut = self
            .auto_cut
            .get(ch)
            .and_then(|x| *x)
            .unwrap_or(channel.cutoff);
        let poly = channel.polyphony;
        let porta = channel.portamento;
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
                        v.slide_rem = 0;
                        v.slide_tot = 0;
                        v.slide_start_hz = start_hz;
                        v.pitch.set_value(start_hz);
                        v.cutoff
                            .set_value((base_cut * cutoff_mul).clamp(40.0, 18000.0));
                        v.gate.set_value(1.0);
                        v.vel = vel.clamp(0.05, 1.0);
                        v.pan = pan.clamp(-1.0, 1.0);
                        v.slide_vel_start = v.vel;
                        v.slide_vel_target = v.vel;
                        v.slide_pan_start = v.pan;
                        v.slide_pan_target = v.pan;
                        v.off = off;
                    }
                    if ch < self.channel_last_hz.len() {
                        self.channel_last_hz[ch] = Some(target_hz);
                    }
                }
            }
            ChannelKind::SuperSynth(_) | ChannelKind::FmSynth(_) => {
                if ch < self.adv_voices.len() {
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
                    if let Some(v) = self.alloc_adv_voice(ch, poly) {
                        v.cutoff_mul = cutoff_mul;
                        v.rel_mul = rel_mul;
                        v.trigger(start_hz, target_hz, glide, vel, pan, off);
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
                head.cutoff_mul = cutoff_mul;
                head.rel_mul = rel_mul;
                self.push_playhead(head);
            }
            ChannelKind::DrumRack { .. } => {
                self.spawn_drum_pad_head(
                    song,
                    midi,
                    HeadSpawn {
                        ch,
                        pitch_midi,
                        vel,
                        pan,
                        cutoff_mul,
                        rel_mul,
                        off,
                    },
                );
            }
            ChannelKind::Sampler { sample } => {
                if let Some(idx) = sample {
                    self.spawn_sampler_head(
                        song,
                        idx,
                        HeadSpawn {
                            ch,
                            pitch_midi,
                            vel,
                            pan,
                            cutoff_mul,
                            rel_mul,
                            off,
                        },
                    );
                }
            }
        }
    }

    fn fire_slide(&mut self, song: &Song, trig: &NoteTrigger, spt: f64) {
        let Some(channel) = song.channels.get(trig.ch) else {
            return;
        };
        let solo_any = song.channels.iter().any(|c| c.solo);
        if (solo_any && !channel.solo) || (!solo_any && channel.mute) {
            return;
        }
        let dur_samples = ((trig.len_ticks.max(1) as f64 * spt) as usize).max(1);
        let pitch_midi = (trig.midi as f32 + trig.fine_pitch).clamp(0.0, 127.0);
        match channel.kind {
            ChannelKind::Synth { .. } => {
                let target_hz = midi_hz(pitch_midi);
                if let Some(pool) = self.voices.get_mut(trig.ch) {
                    for v in pool.iter_mut().filter(|v| v.off > 0) {
                        v.slide_start_hz = v.cur_hz.max(20.0);
                        v.target_hz = target_hz;
                        v.slide_vel_start = v.vel;
                        v.slide_vel_target = trig.vel.clamp(0.05, 1.0);
                        v.slide_pan_start = v.pan;
                        v.slide_pan_target = trig.pan.clamp(-1.0, 1.0);
                        v.slide_tot = dur_samples;
                        v.slide_rem = dur_samples;
                    }
                }
            }
            ChannelKind::SuperSynth(_) | ChannelKind::FmSynth(_) => {
                let target_hz = midi_hz(pitch_midi);
                if let Some(pool) = self.adv_voices.get_mut(trig.ch) {
                    for v in pool.iter_mut().filter(|v| v.off > 0) {
                        v.slide_start_hz = v.cur_hz.max(20.0);
                        v.target_hz = target_hz;
                        v.slide_vel_start = v.vel;
                        v.slide_vel_target = trig.vel.clamp(0.05, 1.0);
                        v.slide_pan_start = v.pan;
                        v.slide_pan_target = trig.pan.clamp(-1.0, 1.0);
                        v.slide_tot = dur_samples;
                        v.slide_rem = dur_samples;
                    }
                }
            }
            ChannelKind::Sampler { .. } => {
                let target_ratio = 2.0_f32.powf((pitch_midi - channel.midi_base as f32) / 12.0);
                for h in self.playheads.iter_mut() {
                    if h.ch == Some(trig.ch) && h.env_sample < h.gate_off {
                        h.slide_start_ratio = h.pitch_ratio.max(0.05);
                        h.slide_target_ratio = target_ratio;
                        h.slide_tot = dur_samples;
                        h.slide_rem = dur_samples;
                    }
                }
            }
            ChannelKind::Kit { .. } | ChannelKind::DrumRack { .. } => {}
        }
    }

    fn render_sample(&mut self, song: &Song) -> (f32, f32) {
        let mut dly_send = 0.0f32;
        let mut rev_send = 0.0f32;
        let sr = self.sr.max(1.0);

        for pair in self.ch_accum.iter_mut() {
            *pair = (0.0, 0.0);
        }
        for pair in self.mix_direct_in.iter_mut() {
            *pair = (0.0, 0.0);
        }
        for pair in self.mix_route_in.iter_mut() {
            *pair = (0.0, 0.0);
        }

        // Accumulate synth voices (fundsp + SuperSynth + FmSynth) per channel.
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
        for (ci, pool) in self.adv_voices.iter_mut().enumerate() {
            let Some(channel) = song.channels.get(ci) else {
                continue;
            };
            let base_cut = self
                .auto_cut
                .get(ci)
                .and_then(|x| *x)
                .unwrap_or(channel.cutoff);
            let mut vl_sum = 0.0f32;
            let mut vr_sum = 0.0f32;
            match &channel.kind {
                ChannelKind::SuperSynth(cfg) => {
                    for v in pool.iter_mut() {
                        let (vl, vr) = v.render_super(channel, cfg, sr, base_cut);
                        vl_sum += vl;
                        vr_sum += vr;
                    }
                }
                ChannelKind::FmSynth(cfg) => {
                    for v in pool.iter_mut() {
                        let (vl, vr) = v.render_fm(channel, cfg, sr, base_cut);
                        vl_sum += vl;
                        vr_sum += vr;
                    }
                }
                _ => {}
            }
            if let Some(acc) = self.ch_accum.get_mut(ci) {
                acc.0 += vl_sum;
                acc.1 += vr_sum;
            }
        }

        // Render PCM playheads (kit / stereo+granular+looping sampler / drum rack / playlist clips).
        let nmix = song.mixer_tracks.len();
        let solo_ch_any = song.channels.iter().any(|c| c.solo);
        let mut i = 0;
        while i < self.playheads.len() {
            let mut done = false;
            {
                let h = &mut self.playheads[i];
                let (data_len, base_ratio, smp_opt) = match h.kind {
                    PlayKind::Kit(p) => {
                        let b = match p {
                            KitPiece::Kick => &self.kit[0],
                            KitPiece::Snare => &self.kit[1],
                            KitPiece::Clap => &self.kit[2],
                            KitPiece::Hat => &self.kit[3],
                        };
                        (b.len(), 1.0, None)
                    }
                    PlayKind::Sample(idx) => match song.samples.get(idx) {
                        Some(s) if !s.data.is_empty() && s.sr > 0.0 => {
                            (s.data.len(), s.sr / sr, Some(s))
                        }
                        _ => (0, 1.0, None),
                    },
                };

                let past_bounds = if data_len == 0 {
                    true
                } else if h.loop_enabled {
                    false
                } else if h.reverse {
                    h.pos < h.start_pos
                } else {
                    (h.pos as usize) >= data_len || h.end_pos.is_some_and(|e| h.pos >= e)
                };

                if past_bounds {
                    done = true;
                } else {
                    if h.slide_rem > 0 && h.slide_tot > 0 {
                        h.slide_rem -= 1;
                        let frac = 1.0 - (h.slide_rem as f32 / h.slide_tot as f32);
                        let start = h.slide_start_ratio.max(0.05);
                        h.pitch_ratio = start * (h.slide_target_ratio / start).powf(frac);
                    }

                    let dir = if h.reverse { -1.0f32 } else { 1.0f32 };
                    let (mut vl, mut vr) = match h.kind {
                        PlayKind::Kit(p) => {
                            let b = match p {
                                KitPiece::Kick => &self.kit[0],
                                KitPiece::Snare => &self.kit[1],
                                KitPiece::Clap => &self.kit[2],
                                KitPiece::Hat => &self.kit[3],
                            };
                            let p0 = (h.pos as usize).min(b.len().saturating_sub(1));
                            let frac = h.pos - p0 as f32;
                            let v0 = b[p0];
                            let v1 = b.get(p0 + 1).copied().unwrap_or(0.0);
                            let s = v0 + (v1 - v0) * frac;
                            h.pos += dir * base_ratio * h.pitch_ratio;
                            (s, s)
                        }
                        PlayKind::Sample(_) => {
                            let smp = smp_opt.expect("checked non-empty");
                            let max_pos = (data_len.saturating_sub(1)) as f32;
                            if h.granular {
                                let grain_size = 1024.0_f32;
                                let p1 = h.grain_phase;
                                let p2 = (h.grain_phase + 0.5) % 1.0;
                                let w1 = 0.5 * (1.0 - (std::f32::consts::TAU * p1).cos());
                                let w2 = 0.5 * (1.0 - (std::f32::consts::TAU * p2).cos());
                                let pos1 = (h.grain_anchor
                                    + (p1 - 0.5) * grain_size * h.pitch_ratio)
                                    .clamp(0.0, max_pos);
                                let pos2 = (h.grain_anchor
                                    + (p2 - 0.5) * grain_size * h.pitch_ratio)
                                    .clamp(0.0, max_pos);
                                let (l1, r1) = read_sample_hermite_lr(smp, pos1);
                                let (l2, r2) = read_sample_hermite_lr(smp, pos2);
                                h.grain_phase =
                                    (h.grain_phase + (h.pitch_ratio / grain_size).max(1e-4)) % 1.0;
                                h.grain_anchor += dir * base_ratio;
                                h.pos = h.grain_anchor;
                                (l1 * w1 + l2 * w2, r1 * w1 + r2 * w2)
                            } else {
                                let (mut sl, mut sr_s) =
                                    read_sample_hermite_lr(smp, h.pos.clamp(0.0, max_pos));
                                if h.loop_enabled && h.loop_xfade > 0.001 {
                                    let loop_span = (h.loop_end - h.loop_start).max(8.0);
                                    let xf_len = (loop_span * h.loop_xfade).max(1.0);
                                    if !h.reverse && h.pos >= h.loop_end - xf_len {
                                        let t = ((h.pos - (h.loop_end - xf_len)) / xf_len)
                                            .clamp(0.0, 1.0);
                                        let xp = (h.loop_start + (h.pos - (h.loop_end - xf_len)))
                                            .clamp(0.0, max_pos);
                                        let (xl, xr) = read_sample_hermite_lr(smp, xp);
                                        sl = sl * (1.0 - t) + xl * t;
                                        sr_s = sr_s * (1.0 - t) + xr * t;
                                    }
                                }
                                h.pos += dir * base_ratio * h.pitch_ratio;
                                (sl, sr_s)
                            }
                        }
                    };

                    if h.loop_enabled {
                        let loop_span = (h.loop_end - h.loop_start).max(1.0);
                        if !h.reverse && h.pos >= h.loop_end {
                            h.pos = h.loop_start + (h.pos - h.loop_end) % loop_span;
                            h.grain_anchor = h.pos;
                        } else if h.reverse && h.pos < h.loop_start {
                            h.pos = h.loop_end - (h.loop_start - h.pos) % loop_span;
                            h.grain_anchor = h.pos;
                        }
                    }

                    vl *= h.gain;
                    vr *= h.gain;

                    if let Some(ci) = h.ch
                        && let Some(channel) = song.channels.get(ci)
                    {
                        let (env_g, env_done) = h.adsr_gain(
                            sr,
                            channel.attack,
                            channel.decay,
                            channel.sustain,
                            channel.release,
                        );
                        let base_cut = self
                            .auto_cut
                            .get(ci)
                            .and_then(|x| *x)
                            .unwrap_or(channel.cutoff);
                        let cut = (base_cut
                            * h.cutoff_mul
                            * 2.0_f32.powf(env_g * h.filter_env_amt * 3.0))
                        .clamp(40.0, 18000.0);
                        let alpha = (2.0 * std::f32::consts::PI * cut / sr).clamp(0.01, 0.99);
                        h.lp_l += alpha * (vl - h.lp_l);
                        h.lp_r += alpha * (vr - h.lp_r);
                        vl = h.lp_l * env_g;
                        vr = h.lp_r * env_g;
                        if env_done {
                            done = true;
                        }
                        let (pl, pr) = pan_gains(h.pan, std::f32::consts::SQRT_2);
                        let out_l = vl * pl;
                        let out_r = vr * pr;
                        if let Some(ins) = h.direct_insert {
                            let muted =
                                (solo_ch_any && !channel.solo) || (!solo_ch_any && channel.mute);
                            if !muted {
                                let vol_auto = self.auto_vol.get(ci).copied().unwrap_or(1.0);
                                let fader = channel.volume * vol_auto;
                                let (cl, cr) = (out_l * fader, out_r * fader);
                                let target = ins.min(nmix.saturating_sub(1));
                                if let Some(m) = self.mix_direct_in.get_mut(target) {
                                    m.0 += cl;
                                    m.1 += cr;
                                }
                                if ci < self.ch_peaks.len() {
                                    self.ch_peaks[ci] =
                                        self.ch_peaks[ci].max(cl.abs().max(cr.abs()));
                                }
                            }
                        } else if let Some(acc) = self.ch_accum.get_mut(ci) {
                            acc.0 += out_l;
                            acc.1 += out_r;
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
                        let scale = 0.9 * fade;
                        let target = h.mixer_insert.min(nmix.saturating_sub(1));
                        if let Some(m) = self.mix_direct_in.get_mut(target) {
                            m.0 += vl * scale;
                            m.1 += vr * scale;
                        }
                    }
                }
            }
            if done {
                self.playheads.swap_remove(i);
            } else {
                i += 1;
            }
        }

        // Pull live audio input stream and optionally monitor into armed insert.
        let (in_l, in_r) = if let Some(rx) = self.input_rx.as_mut() {
            if rx.slots() >= 2 {
                let l_in = rx.pop().unwrap_or(0.0);
                let r_in = rx.pop().unwrap_or(l_in);
                (l_in, r_in)
            } else {
                (0.0, 0.0)
            }
        } else {
            (0.0, 0.0)
        };
        self.last_input = (in_l, in_r);
        self.input_peak = (self.input_peak * 0.94).max(in_l.abs().max(in_r.abs()));
        if self.input_monitor && nmix > 0 {
            let target = self.input_insert.min(nmix - 1);
            self.mix_direct_in[target].0 += in_l;
            self.mix_direct_in[target].1 += in_r;
        }

        // Smooth channel controls, pan, feed mixer inserts, meter, and accumulate sends.
        let smooth_k = (220.0 / sr).clamp(0.001, 0.25);
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

            let muted = (solo_ch_any && !channel.solo) || (!solo_ch_any && channel.mute);
            let vol_auto = self.auto_vol.get(ci).copied().unwrap_or(1.0);
            let pan_auto = self.auto_pan.get(ci).copied().unwrap_or(0.0);
            let fader = if muted { 0.0 } else { vol_s * vol_auto };
            let (lg, rg) = pan_gains((pan_s + pan_auto).clamp(-1.0, 1.0), fader);
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

        // Process Mixer Insert DAG in topological order with automatic PDC and 10-slot FX rack.
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        let solo_inserts = song.mixer_tracks.iter().skip(1).any(|m| m.solo);
        self.fft_tick = self.fft_tick.wrapping_add(1);
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
            let post_fx = if t < self.mix_fx.len() {
                self.mix_fx[t].process_insert_fx(
                    track,
                    sr,
                    song.bpm,
                    post_lat.0,
                    post_lat.1,
                    &self.mix_sc_bus,
                )
            } else {
                post_lat
            };
            if t < self.mix_sc_bus.len() {
                self.mix_sc_bus[t] = post_fx;
            }

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
            let auto_p = self.mix_auto_pan.get(t).copied().unwrap_or(0.0);
            let gain = if is_muted { 0.0 } else { t_vol * auto_g };
            let (gl, gr) = pan_gains(
                (t_pan + auto_p).clamp(-1.0, 1.0),
                gain * std::f32::consts::SQRT_2,
            );
            let tl = post_fx.0 * gl;
            let tr = post_fx.1 * gr;

            if t < self.mix_peaks.len() {
                self.mix_peaks[t] = (self.mix_peaks[t] * 0.94).max(tl.abs().max(tr.abs()));
            }
            if t < self.mix_fx.len() {
                let rms = self.mix_fx[t].update_meters(tl, tr);
                if t < self.mix_rms.len() {
                    self.mix_rms[t] = rms;
                }
                if t < self.mix_clips.len() && self.mix_fx[t].clip_hold > 0 {
                    self.mix_clips[t] = true;
                }
                if t == self.inspect_insert {
                    for (si, slot_rt) in self.mix_fx[t].slots.iter().enumerate().take(MAX_FX_SLOTS)
                    {
                        self.comp_gr_db[si] = slot_rt.last_gr_db;
                    }
                    if self.fft_tick.is_multiple_of(128) {
                        compute_fft_bins(
                            &self.mix_fx[t].spec_ring,
                            self.mix_fx[t].spec_pos,
                            &mut self.fft_bins,
                        );
                    }
                }
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

        // Peak Controllers + internal LFO modulate parameters smoothly at audio rate.
        for (pi, pc) in song.peak_controllers.iter().enumerate() {
            if !pc.enabled || pi >= self.peak_ctrl_env.len() {
                continue;
            }
            let src = self
                .mix_sc_bus
                .get(pc.source_insert)
                .map(|(sl, sr_v)| sl.abs().max(sr_v.abs()))
                .unwrap_or(0.0);
            let env = &mut self.peak_ctrl_env[pi];
            if src > *env {
                *env += (src - *env) * 0.08;
            } else {
                let rel = 1.0 - (-1.0 / (pc.decay_ms.max(5.0) * 0.001 * sr)).exp();
                *env += (src - *env) * rel;
            }
            let env_val = *env;
            let phase = &mut self.peak_ctrl_lfo[pi];
            *phase = (*phase + pc.lfo_hz.clamp(0.0, 30.0) / sr) % 1.0;
            let lfo_val = (*phase * std::f32::consts::TAU).sin() * pc.lfo_amt;
            let val = pc.base + env_val * pc.amount + lfo_val;
            self.apply_auto_target(pc.target, val);
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

        let in_punch = song
            .punch_in_tick
            .is_none_or(|pi| self.tick_pos >= pi as f64)
            && song
                .punch_out_tick
                .is_none_or(|po| self.tick_pos < po as f64);
        if self.recording && self.playing && self.count_in_rem_samples == 0 && in_punch {
            let (rec_l, rec_r) = if self.input_rec_only {
                self.last_input
            } else {
                (l, r)
            };
            if let Some(tx) = &mut self.take_tx {
                if tx.slots() >= 2 {
                    let _ = tx.push(rec_l);
                    let _ = tx.push(rec_r);
                    self.take_samples = self.take_samples.saturating_add(1);
                } else {
                    self.xruns = self.xruns.saturating_add(1);
                }
            } else if self.input_rec_only {
                if self.take_in_l.len() < 44100 * 60 * 5 {
                    self.take_in_l.push(rec_l);
                    self.take_in_r.push(rec_r);
                    self.take_samples = self.take_in_l.len() as u64;
                } else {
                    self.recording = false;
                    self.take_full = true;
                }
            } else if self.take_l.len() >= 44100 * 60 * 5 {
                self.recording = false;
                self.take_full = true;
            } else {
                self.take_l.push(rec_l);
                self.take_r.push(rec_r);
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
        self.count_in_rem_samples = 0;
        for pool in self.voices.iter_mut() {
            for v in pool.iter_mut() {
                v.gate.set_value(0.0);
                v.off = 0;
                v.live_midi = None;
            }
        }
        for pool in self.adv_voices.iter_mut() {
            for v in pool.iter_mut() {
                v.kill();
            }
        }
        self.midi_sustain = false;
        self.midi_sustain_pending = [false; 128];
        self.live_voice.gate.set_value(0.0);
        self.live_off = 0;
        self.playheads.clear();
        self.metro_heads.clear();
        self.pending.clear();
        self.recording = false;
        self.master_auto_vol = None;
        self.auto_vol.fill(1.0);
        self.auto_pan.fill(0.0);
        self.mix_auto_vol.fill(1.0);
        self.mix_auto_pan.fill(0.0);
        self.mix_sc_bus.fill((0.0, 0.0));
        self.peak_ctrl_env.fill(0.0);
        self.peak_ctrl_lfo.fill(0.0);
        for m in self.mix_fx.iter_mut() {
            m.reset();
        }
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
        self.mix_rms.fill(0.0);
        self.mix_clips.fill(false);
        self.fft_bins.fill(-72.0);
        self.comp_gr_db.fill(0.0);
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
    ArmCountIn,
    ClearTake,
    RebuildChannel(usize),
    SetChannelCutoff(usize, f32),
    SetInspectInsert(usize),
    ClearMixClips,
    LiveNote { ch: usize, midi: u8, vel: f32 },
    LiveKit(KitPiece),
    LiveSample(usize),
    Midi(MidiMsg),
    SetMidiTarget(usize),
    SetInputMonitor { enabled: bool, insert: usize },
    SetInputRecOnly(bool),
    SetInputRx(Option<Box<rtrb::Consumer<f32>>>),
}

struct AudioTelemetry {
    playing: AtomicBool,
    recording: AtomicBool,
    take_full: AtomicBool,
    cur_pat: AtomicUsize,
    cur_tick: AtomicU32,
    tick_pos_bits: AtomicU64,
    peak_bits: AtomicU32,
    input_peak_bits: AtomicU32,
    xruns: AtomicU64,
    take_samples: AtomicU64,
    ack_seq: AtomicU64,
    ch_peaks: [AtomicU32; MAX_TELEM_CH],
    mix_peaks: [AtomicU32; MAX_TELEM_CH],
    mix_rms: [AtomicU32; MAX_TELEM_CH],
    mix_clips: [AtomicBool; MAX_TELEM_CH],
    fft_bins: [AtomicU32; FFT_BINS],
    comp_gr_db: [AtomicU32; MAX_FX_SLOTS],
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
            input_peak_bits: AtomicU32::new(0.0_f32.to_bits()),
            xruns: AtomicU64::new(0),
            take_samples: AtomicU64::new(0),
            ack_seq: AtomicU64::new(0),
            ch_peaks: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
            mix_peaks: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
            mix_rms: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
            mix_clips: std::array::from_fn(|_| AtomicBool::new(false)),
            fft_bins: std::array::from_fn(|_| AtomicU32::new((-72.0_f32).to_bits())),
            comp_gr_db: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
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
    midi_rt_tx: Option<rtrb::Producer<MidiMsg>>,
}

impl AudioBridge {
    fn send(&mut self, cmd: AudioCmd) {
        if self.cmd_tx.push(cmd).is_ok() {
            self.sent_seq = self.sent_seq.wrapping_add(1);
        } else {
            self.telemetry.xruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn take_midi_rt_tx(&mut self) -> Option<rtrb::Producer<MidiMsg>> {
        self.midi_rt_tx.take()
    }
}

struct AudioWorker {
    song: Arc<Song>,
    rt: EngineRt,
    cmd_rx: rtrb::Consumer<AudioCmd>,
    recycle_tx: rtrb::Producer<Arc<Song>>,
    midi_rx: rtrb::Consumer<MidiMsg>,
    telemetry: Arc<AudioTelemetry>,
    bl: Vec<f32>,
    br: Vec<f32>,
}

impl AudioWorker {
    fn drain_commands(&mut self) {
        while let Ok(msg) = self.midi_rx.pop() {
            self.rt.handle_midi_msg(&self.song, msg);
        }
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
                AudioCmd::ArmCountIn => self.rt.arm_count_in(&self.song),
                AudioCmd::ClearTake => {
                    self.rt.take_full = false;
                    self.rt.take_samples = 0;
                }
                AudioCmd::RebuildChannel(ch) => self.rt.rebuild_channel(&self.song, ch),
                AudioCmd::SetChannelCutoff(ch, v) => self.rt.set_channel_cutoff(ch, v),
                AudioCmd::SetInspectInsert(ins) => self.rt.inspect_insert = ins,
                AudioCmd::ClearMixClips => {
                    self.rt.mix_clips.fill(false);
                    for m in self.rt.mix_fx.iter_mut() {
                        m.clip_hold = 0;
                    }
                }
                AudioCmd::LiveNote { ch, midi, vel } => {
                    self.rt.live_note_ch(&self.song, ch, midi, vel);
                }
                AudioCmd::LiveKit(p) => self.rt.live_kit(p),
                AudioCmd::LiveSample(idx) => self.rt.live_sample(idx),
                AudioCmd::Midi(msg) => self.rt.handle_midi_msg(&self.song, msg),
                AudioCmd::SetMidiTarget(ch) => self.rt.midi_target_ch = ch,
                AudioCmd::SetInputMonitor { enabled, insert } => {
                    self.rt.input_monitor = enabled;
                    self.rt.input_insert = insert;
                }
                AudioCmd::SetInputRecOnly(v) => self.rt.input_rec_only = v,
                AudioCmd::SetInputRx(rx) => self.rt.input_rx = rx.map(|b| *b),
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
        t.input_peak_bits
            .store(self.rt.input_peak.to_bits(), Ordering::Relaxed);
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
        for (i, r) in self.rt.mix_rms.iter().take(MAX_TELEM_CH).enumerate() {
            t.mix_rms[i].store(r.to_bits(), Ordering::Relaxed);
        }
        for (i, c) in self.rt.mix_clips.iter().take(MAX_TELEM_CH).enumerate() {
            t.mix_clips[i].store(*c, Ordering::Relaxed);
        }
        for (i, b) in self.rt.fft_bins.iter().take(FFT_BINS).enumerate() {
            t.fft_bins[i].store(b.to_bits(), Ordering::Relaxed);
        }
        for (i, g) in self.rt.comp_gr_db.iter().take(MAX_FX_SLOTS).enumerate() {
            t.comp_gr_db[i].store(g.to_bits(), Ordering::Relaxed);
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
    pub next_take_seq: u32,
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
            next_take_seq: 1,
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
        self.rt.input_peak = f32::from_bits(t.input_peak_bits.load(Ordering::Relaxed));
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
        if self.rt.mix_rms.len() != self.song.mixer_tracks.len() {
            self.rt.mix_rms.resize(self.song.mixer_tracks.len(), 0.0);
        }
        if self.rt.mix_clips.len() != self.song.mixer_tracks.len() {
            self.rt
                .mix_clips
                .resize(self.song.mixer_tracks.len(), false);
        }
        for i in 0..nmix {
            self.rt.mix_peaks[i] = f32::from_bits(t.mix_peaks[i].load(Ordering::Relaxed));
            self.rt.mix_rms[i] = f32::from_bits(t.mix_rms[i].load(Ordering::Relaxed));
            self.rt.mix_clips[i] = t.mix_clips[i].load(Ordering::Relaxed);
        }
        for i in 0..FFT_BINS {
            self.rt.fft_bins[i] = f32::from_bits(t.fft_bins[i].load(Ordering::Relaxed));
        }
        for i in 0..MAX_FX_SLOTS {
            self.rt.comp_gr_db[i] = f32::from_bits(t.comp_gr_db[i].load(Ordering::Relaxed));
        }
        for (i, s) in self.rt.scope.iter_mut().enumerate().take(SCOPE_LEN) {
            *s = f32::from_bits(t.scope[i].load(Ordering::Relaxed));
        }
        self.rt.scope_pos = 0;
    }

    pub fn set_midi_target(&mut self, ch: usize) {
        self.rt.midi_target_ch = ch;
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetMidiTarget(ch));
        }
    }

    pub fn send_midi_msg(&mut self, msg: MidiMsg) {
        if self.audio.is_none() {
            self.rt.handle_midi_msg(&self.song, msg);
        } else if let Some(b) = &mut self.audio {
            b.send(AudioCmd::Midi(msg));
        }
    }

    pub fn set_input_monitor(&mut self, enabled: bool, insert: usize) {
        self.rt.input_monitor = enabled;
        self.rt.input_insert = insert;
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetInputMonitor { enabled, insert });
        }
    }

    pub fn set_input_rec_only(&mut self, v: bool) {
        self.rt.input_rec_only = v;
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetInputRecOnly(v));
        }
    }

    pub fn set_input_consumer(&mut self, rx: Option<rtrb::Consumer<f32>>) {
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetInputRx(rx.map(Box::new)));
        } else {
            self.rt.input_rx = rx;
        }
    }

    pub fn set_inspect_insert(&mut self, ins: usize) {
        self.rt.inspect_insert = ins;
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::SetInspectInsert(ins));
        }
    }

    pub fn clear_mix_clips(&mut self) {
        self.rt.mix_clips.fill(false);
        for m in self.rt.mix_fx.iter_mut() {
            m.clip_hold = 0;
        }
        if let Some(b) = &mut self.audio {
            b.send(AudioCmd::ClearMixClips);
        }
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
            self.rt.take_in_l.clear();
            self.rt.take_in_r.clear();
            self.rt.take_full = false;
            self.rt.take_samples = 0;
            if let Some(b) = &mut self.audio {
                b.take_disk.clear_flag.store(true, Ordering::Release);
                b.send(AudioCmd::ClearTake);
            }
        }
        if rec {
            self.rt.arm_count_in(&self.song);
            self.rt.playing = true;
            if let Some(b) = &mut self.audio {
                b.send(AudioCmd::ArmCountIn);
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
        } else if self.rt.input_rec_only {
            self.rt.take_in_l.len() as u64
        } else {
            self.rt.take_l.len() as u64
        };
        (n / self.rt.sr.max(1.0) as u64) as usize
    }

    pub fn has_take(&self) -> bool {
        if let Some(b) = &self.audio {
            b.take_disk.written_samples.load(Ordering::Relaxed) > 0
        } else {
            !self.rt.take_l.is_empty() || !self.rt.take_in_l.is_empty()
        }
    }

    pub fn save_take(&self, path: &str) -> Result<usize, String> {
        let Some(b) = &self.audio else {
            let (l, r) = if self.rt.input_rec_only && !self.rt.take_in_l.is_empty() {
                (&self.rt.take_in_l, &self.rt.take_in_r)
            } else {
                (&self.rt.take_l, &self.rt.take_r)
            };
            return crate::export::write_wav_file(path, l, r, self.rt.sr);
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

    /// Write current recorded take to project takes directory, import into `Song::samples`,
    /// and place an AudioClip on the Playlist at `(track, start_tick)`.
    pub fn commit_audio_take(&mut self, track: usize, start_tick: u32) -> Result<u64, String> {
        let dir = crate::midi::takes_dir();
        let name = format!("take_{:03}.wav", self.next_take_seq);
        self.next_take_seq = self.next_take_seq.saturating_add(1);
        let path = dir.join(&name);
        let path_str = path.to_string_lossy().into_owned();
        let frames = self.save_take(&path_str)?;
        if frames == 0 {
            return Err("nothing recorded yet".into());
        }
        let s_idx = sample_into_song(self, &path_str)?;
        let clip_id = self.song.add_audio_clip(track, start_tick, s_idx, 1.0);
        let spt = (self.rt.sr.max(1.0) * 60.0 / self.song.bpm.max(30.0) / 16.0).max(1.0);
        let ticks = ((frames as f32) / spt).ceil().max(4.0) as u32;
        if let Some(c) = self.song.clips.iter_mut().find(|c| c.id == clip_id) {
            c.len_ticks = ticks;
        }
        Ok(clip_id)
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
    let (data, data_r, sr) = load_sample_file(path)?;
    let name = std::path::Path::new(path)
        .file_name()
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let sample_ref = if data_r.is_empty() {
        SampleRef::from_mono(name, path, data, sr)
    } else {
        SampleRef::from_stereo(name, path, data, data_r, sr)
    };
    s.song.samples.push(sample_ref);
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
    let (midi_rt_tx, midi_rx) = rtrb::RingBuffer::<MidiMsg>::new(512);

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
        midi_rx,
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
        midi_rt_tx: Some(midi_rt_tx),
    });
    Some(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::song::{
        CompCfg, EffectSlot, EqCfg, FmSynthCfg, FxKind, SuperSynthCfg, default_drum_pads,
    };

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
        song.samples.push(SampleRef::from_mono(
            "sine.wav",
            "sine.wav",
            (0..4410).map(|i| (i as f32 * 0.1).sin()).collect(),
            44100.0,
        ));
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
        let (_midi_tx, midi_rx) = rtrb::RingBuffer::<MidiMsg>::new(16);
        let telemetry = Arc::new(AudioTelemetry::new());
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        let mut worker = AudioWorker {
            song: Arc::new(song.clone()),
            rt,
            cmd_rx,
            recycle_tx,
            midi_rx,
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
        song.samples.push(SampleRef::from_mono(
            "tone.wav",
            "tone.wav",
            vec![0.5; 4410],
            44100.0,
        ));
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
        song.samples
            .push(SampleRef::from_mono("imp.wav", "imp.wav", imp, 44100.0));
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

    #[test]
    fn slide_note_glides_active_synth_pitch() {
        let mut song = crate::song::new_empty();
        // Sustain C4 (MIDI 60) for 16 ticks, and glide up to C5 (MIDI 72) over ticks 4..12.
        song.patterns[0].notes[4].push(crate::song::Note::new(0, 16, 60, 0.9));
        let mut slide = crate::song::Note::new(4, 8, 72, 0.9);
        slide.slide = true;
        song.patterns[0].notes[4].push(slide);
        let mut rt = EngineRt::new(44100.0);
        rt.playing = true;
        // At 128 BPM (~1292 samples/tick), ticks 0..14 take ~18088 samples.
        let mut l = vec![0.0; 18500];
        let mut r = vec![0.0; 18500];
        rt.process(&song, &mut l, &mut r);
        let c5_hz = midi_hz(72.0);
        let active_cur = rt.voices[4]
            .iter()
            .find(|v| v.off > 0)
            .map(|v| v.cur_hz)
            .expect("synth note should still be sustaining at tick 14");
        assert!(
            (active_cur - c5_hz).abs() < 5.0,
            "expected glide to reach C5 ({c5_hz:.1} Hz), got {active_cur:.1} Hz"
        );
    }

    #[test]
    fn muted_note_produces_no_audio() {
        let mut song = crate::song::new_empty();
        let mut muted = crate::song::Note::new(0, 8, 60, 1.0);
        muted.mute = true;
        song.patterns[0].notes[4].push(muted);
        let mut rt = EngineRt::new(44100.0);
        rt.playing = true;
        let mut l = vec![0.0; 4096];
        let mut r = vec![0.0; 4096];
        rt.process(&song, &mut l, &mut r);
        let peak = l.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        assert!(peak < 1e-6, "muted note must emit zero audio, got {peak}");
    }

    #[test]
    fn supersynth_fm_and_drumrack_render_clean_stereo() {
        let mut song = crate::song::new_empty();
        song.channels[4].kind = ChannelKind::SuperSynth(SuperSynthCfg {
            unison: 5,
            detune: 0.4,
            spread: 0.85,
            ..Default::default()
        });
        song.channels[5].kind = ChannelKind::FmSynth(FmSynthCfg::default());
        song.channels[6].kind = ChannelKind::DrumRack {
            pads: default_drum_pads(),
        };
        song.patterns[0].notes[4].push(crate::song::Note::new(0, 16, 60, 0.9));
        song.patterns[0].notes[5].push(crate::song::Note::new(0, 16, 48, 0.9));
        song.patterns[0].notes[6].push(crate::song::Note::new(0, 4, 36, 0.9));
        let mut rt = EngineRt::new(44100.0);
        rt.playing = true;
        let mut l = vec![0.0; 4096];
        let mut r = vec![0.0; 4096];
        rt.process(&song, &mut l, &mut r);
        let peak = l.iter().chain(&r).map(|x| x.abs()).fold(0.0f32, f32::max);
        let diff: f32 = l.iter().zip(&r).map(|(a, b)| (a - b).abs()).sum();
        assert!(peak > 0.02, "expected audible output, got {peak}");
        assert!(diff > 0.5, "expected stereo image from SuperSynth spread");
        assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    }

    #[test]
    fn sampler_reverse_loop_and_granular_render_cleanly() {
        let mut song = crate::song::new_empty();
        let left: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.05).sin() * 0.8).collect();
        let right: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.05).cos() * 0.8).collect();
        song.samples.push(SampleRef::from_stereo(
            "st.wav", "st.wav", left, right, 44100.0,
        ));
        song.channels[7].kind = ChannelKind::Sampler { sample: Some(0) };
        song.channels[7].sampler.reverse = true;
        song.channels[7].sampler.loop_enabled = true;
        song.channels[7].sampler.stretch = StretchMode::Granular;
        song.channels[7].cutoff = 16000.0;
        song.patterns[0].notes[7].push(crate::song::Note::new(0, 16, 60, 1.0));
        let mut rt = EngineRt::new(44100.0);
        rt.playing = true;
        let mut l = vec![0.0; 4096];
        let mut r = vec![0.0; 4096];
        rt.process(&song, &mut l, &mut r);
        let peak = l.iter().chain(&r).map(|x| x.abs()).fold(0.0f32, f32::max);
        assert!(peak > 0.01, "granular reverse looping sampler silent");
        assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    }

    #[test]
    fn mixer_fx_rack_sidechain_and_fft_work() {
        let mut song = crate::song::new_empty();
        // Channel 0 -> Insert 2 (sidechain source)
        // Channel 4 -> Insert 1 (compressed by Insert 2, EQed, and inspected for FFT/GR)
        song.channels[0].mixer_insert = 2;
        song.channels[4].mixer_insert = 1;
        song.mixer_tracks[1].add_fx(EffectSlot {
            name: "EQ".into(),
            bypass: false,
            mix: 1.0,
            kind: FxKind::ParametricEq(EqCfg::default()),
        });
        song.mixer_tracks[1].add_fx(EffectSlot {
            name: "SC Comp".into(),
            bypass: false,
            mix: 1.0,
            kind: FxKind::Compressor(CompCfg {
                threshold_db: -28.0,
                ratio: 8.0,
                attack_ms: 1.0,
                release_ms: 40.0,
                makeup_db: 0.0,
                knee_db: 3.0,
                sidechain_from: Some(2),
                limiter: false,
                lookahead_samples: 0,
            }),
        });
        song.mixer_tracks[2].routes.clear();
        song.mixer_tracks[2].routes.push(crate::song::MixerRoute {
            target: 0,
            amount: 1.0,
            sidechain_only: false,
        });
        song.patterns[0].notes[0].push(crate::song::Note::new(0, 4, 36, 1.0));
        song.patterns[0].notes[4].push(crate::song::Note::new(0, 16, 48, 1.0));
        let mut rt = EngineRt::new(44100.0);
        rt.inspect_insert = 1;
        rt.playing = true;
        let mut l = vec![0.0; 2048];
        let mut r = vec![0.0; 2048];
        rt.process(&song, &mut l, &mut r);
        assert!(
            rt.comp_gr_db[1] < -0.05,
            "expected sidechain GR, got {}",
            rt.comp_gr_db[1]
        );
        assert!(
            rt.fft_bins.iter().any(|&b| b > -70.0),
            "expected active FFT bins on inspected insert"
        );
        assert!(rt.mix_rms[1] > 0.001, "expected RMS meter activity");
    }

    #[test]
    fn phase5_midi_sustain_pitchbend_and_modwheel() {
        let mut song = crate::song::new_empty();
        song.channels[4].kind = ChannelKind::SuperSynth(SuperSynthCfg::default());
        let mut rt = EngineRt::new(44100.0);
        rt.ensure_channels(&song);
        rt.midi_target_ch = 4;

        rt.handle_midi_msg(
            &song,
            MidiMsg::Cc {
                ch: 0,
                cc: 64,
                val: 127,
            },
        );
        rt.handle_midi_msg(
            &song,
            MidiMsg::NoteOn {
                ch: 0,
                note: 60,
                vel: 0.9,
            },
        );
        rt.handle_midi_msg(&song, MidiMsg::NoteOff { ch: 0, note: 60 });
        assert!(
            rt.midi_sustain_pending[60],
            "sustain pedal must hold note off"
        );
        rt.handle_midi_msg(&song, MidiMsg::PitchBend { ch: 0, norm: 0.5 });
        rt.handle_midi_msg(
            &song,
            MidiMsg::Cc {
                ch: 0,
                cc: 1,
                val: 127,
            },
        );
        assert!((rt.midi_bend_semi - 1.0).abs() < 1e-4);
        assert!((rt.midi_mod_wheel - 1.0).abs() < 1e-4);

        let mut l = vec![0.0; 1024];
        let mut r = vec![0.0; 1024];
        rt.process(&song, &mut l, &mut r);
        assert!(l.iter().chain(&r).any(|x| x.abs() > 0.005));

        rt.handle_midi_msg(
            &song,
            MidiMsg::Cc {
                ch: 0,
                cc: 64,
                val: 0,
            },
        );
        assert!(!rt.midi_sustain_pending[60]);
    }

    #[test]
    fn phase5_sample_accurate_automation_curves_and_peak_controller() {
        use crate::song::{AutoClipPoint, ClipKind, CurveMode, PeakController};

        let mut song = crate::song::new_empty();
        // Hold MasterVol at 0.0 for the first 16 ticks, then jump to 1.4.
        let cid = song.add_auto_clip(0, 0, 64, crate::song::AutoTarget::MasterVol);
        if let Some(c) = song.clips.iter_mut().find(|c| c.id == cid)
            && let ClipKind::Automation { points, .. } = &mut c.kind
        {
            *points = vec![
                AutoClipPoint {
                    tick: 0,
                    value: 0.0,
                    tension: 0.0,
                    curve: CurveMode::Hold,
                },
                AutoClipPoint {
                    tick: 16,
                    value: 1.4,
                    tension: 0.0,
                    curve: CurveMode::Linear,
                },
            ];
        }
        song.patterns[0].notes[4].push(crate::song::Note::new(0, 32, 60, 1.0));
        song.add_pattern_clip(1, 0, 0);

        let mut rt = EngineRt::new(44100.0);
        rt.song_mode = true;
        rt.playing = true;
        // First 4096 samples lie inside ticks 0..4 (< tick 16), so Hold=0.0 mutes Master.
        let mut l0 = vec![0.0; 4096];
        let mut r0 = vec![0.0; 4096];
        rt.process(&song, &mut l0, &mut r0);
        let peak_hold = l0.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        assert!(
            peak_hold < 1e-5,
            "hold automation should stay 0.0, got {peak_hold}"
        );

        // PeakController: Insert 1 (kick) ducks Insert 5 (bass) volume.
        let mut song2 = crate::song::new_empty();
        song2.patterns[0].notes[0].push(crate::song::Note::new(0, 4, 36, 1.0));
        song2.peak_controllers.push(PeakController {
            name: "Duck".into(),
            source_insert: 1,
            target: crate::song::AutoTarget::InsertVol(5),
            amount: -0.9,
            base: 1.0,
            decay_ms: 80.0,
            lfo_hz: 0.0,
            lfo_amt: 0.0,
            enabled: true,
        });
        let mut rt2 = EngineRt::new(44100.0);
        rt2.playing = true;
        let mut l = vec![0.0; 2048];
        let mut r = vec![0.0; 2048];
        rt2.process(&song2, &mut l, &mut r);
        assert!(
            rt2.mix_auto_vol[5] < 0.99,
            "expected PeakController to duck insert 5, got {}",
            rt2.mix_auto_vol[5]
        );
    }

    #[test]
    fn phase5_audio_input_monitoring_and_punch_recording() {
        let mut song = crate::song::new_empty();
        song.punch_in_tick = Some(4);
        song.punch_out_tick = Some(12);
        let (mut in_tx, in_rx) = rtrb::RingBuffer::<f32>::new(65536);
        for _ in 0..25000 {
            let _ = in_tx.push(0.4);
            let _ = in_tx.push(-0.4);
        }
        let mut rt = EngineRt::new(44100.0);
        rt.input_rx = Some(in_rx);
        rt.input_monitor = true;
        rt.input_insert = 1;
        rt.input_rec_only = true;
        rt.playing = true;
        rt.recording = true;

        // ~1292 samples/tick at 128 BPM -> 24000 samples spans ticks 0..18 (crosses punch 4..12).
        let mut l = vec![0.0; 24000];
        let mut r = vec![0.0; 24000];
        rt.process(&song, &mut l, &mut r);
        assert!(rt.input_peak > 0.2, "expected live input meter activity");
        assert!(
            l.iter().any(|x| x.abs() > 0.01),
            "expected monitored input audio on master output"
        );
        assert!(
            rt.take_in_l.len() > 2000 && rt.take_in_l.len() < 20000,
            "expected punch-gated take length, got {}",
            rt.take_in_l.len()
        );
    }
}
