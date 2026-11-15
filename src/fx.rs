//! Mixer DSP FX processors (7-band Parametric EQ + FFT analyzer, Compressor /
//! brickwall Limiter w/ sidechain, Tempo-Synced Delay, Algorithmic Reverb,
//! Chorus/Flanger/Phaser, Saturator, M/S Stereo Shaper) and dB helpers.

use crate::song::{
    CompCfg, DelayFxCfg, EqBand, EqBandType, EqCfg, FxKind, MAX_FX_SLOTS, MixerTrack, ModFxCfg,
    ModFxKind, ReverbFxCfg, SatCfg, SatKind, StereoShaperCfg,
};
use std::f32::consts::{PI, TAU};

pub const FFT_N: usize = 256;
pub const FFT_BINS: usize = 64;

pub fn db_to_gain(db: f32) -> f32 {
    if db <= -72.0 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 1e-5 {
        -96.0
    } else {
        20.0 * gain.log10()
    }
}

pub fn format_db(gain: f32) -> String {
    if gain <= 1e-4 {
        "-inf dB".into()
    } else {
        format!("{:+.1} dB", gain_to_db(gain))
    }
}

// ---------------------------------------------------------------------------
// RBJ Biquad & 7-Band Parametric EQ
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct BiquadCoeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Default for BiquadCoeffs {
    fn default() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }
}

fn design_biquad(band: &EqBand, sr: f32) -> BiquadCoeffs {
    if !band.enabled {
        return BiquadCoeffs::default();
    }
    let sr = sr.max(8000.0);
    let freq = band.freq_hz.clamp(20.0, sr * 0.46);
    let q = band.q.clamp(0.2, 12.0);
    let w0 = TAU * freq / sr;
    let cos_w = w0.cos();
    let sin_w = w0.sin();
    let alpha = sin_w / (2.0 * q);
    let a_lin = 10.0_f32.powf(band.gain_db.clamp(-24.0, 24.0) / 40.0);

    let (b0, b1, b2, a0, a1, a2) = match band.kind {
        EqBandType::HighPass => (
            (1.0 + cos_w) * 0.5,
            -(1.0 + cos_w),
            (1.0 + cos_w) * 0.5,
            1.0 + alpha,
            -2.0 * cos_w,
            1.0 - alpha,
        ),
        EqBandType::LowPass => (
            (1.0 - cos_w) * 0.5,
            1.0 - cos_w,
            (1.0 - cos_w) * 0.5,
            1.0 + alpha,
            -2.0 * cos_w,
            1.0 - alpha,
        ),
        EqBandType::Bell => (
            1.0 + alpha * a_lin,
            -2.0 * cos_w,
            1.0 - alpha * a_lin,
            1.0 + alpha / a_lin,
            -2.0 * cos_w,
            1.0 - alpha / a_lin,
        ),
        EqBandType::LowShelf => {
            let two_sqrt_a_alpha = 2.0 * a_lin.sqrt() * alpha;
            (
                a_lin * ((a_lin + 1.0) - (a_lin - 1.0) * cos_w + two_sqrt_a_alpha),
                2.0 * a_lin * ((a_lin - 1.0) - (a_lin + 1.0) * cos_w),
                a_lin * ((a_lin + 1.0) - (a_lin - 1.0) * cos_w - two_sqrt_a_alpha),
                (a_lin + 1.0) + (a_lin - 1.0) * cos_w + two_sqrt_a_alpha,
                -2.0 * ((a_lin - 1.0) + (a_lin + 1.0) * cos_w),
                (a_lin + 1.0) + (a_lin - 1.0) * cos_w - two_sqrt_a_alpha,
            )
        }
        EqBandType::HighShelf => {
            let two_sqrt_a_alpha = 2.0 * a_lin.sqrt() * alpha;
            (
                a_lin * ((a_lin + 1.0) + (a_lin - 1.0) * cos_w + two_sqrt_a_alpha),
                -2.0 * a_lin * ((a_lin - 1.0) + (a_lin + 1.0) * cos_w),
                a_lin * ((a_lin + 1.0) + (a_lin - 1.0) * cos_w - two_sqrt_a_alpha),
                (a_lin + 1.0) - (a_lin - 1.0) * cos_w + two_sqrt_a_alpha,
                2.0 * ((a_lin - 1.0) - (a_lin + 1.0) * cos_w),
                (a_lin + 1.0) - (a_lin - 1.0) * cos_w - two_sqrt_a_alpha,
            )
        }
    };
    let inv_a0 = 1.0 / a0.max(1e-8);
    BiquadCoeffs {
        b0: b0 * inv_a0,
        b1: b1 * inv_a0,
        b2: b2 * inv_a0,
        a1: a1 * inv_a0,
        a2: a2 * inv_a0,
    }
}

pub fn eq_response_db(cfg: &EqCfg, freq_hz: f32, sr: f32) -> f32 {
    let w = TAU * freq_hz.clamp(20.0, sr * 0.46) / sr.max(8000.0);
    let cos_w = w.cos();
    let cos_2w = (2.0 * w).cos();
    let sin_w = w.sin();
    let sin_2w = (2.0 * w).sin();
    let mut mag_sq = 1.0f32;
    for b in &cfg.bands {
        if !b.enabled {
            continue;
        }
        let c = design_biquad(b, sr);
        let num_re = c.b0 + c.b1 * cos_w + c.b2 * cos_2w;
        let num_im = -c.b1 * sin_w - c.b2 * sin_2w;
        let den_re = 1.0 + c.a1 * cos_w + c.a2 * cos_2w;
        let den_im = -c.a1 * sin_w - c.a2 * sin_2w;
        let num = num_re * num_re + num_im * num_im;
        let den = (den_re * den_re + den_im * den_im).max(1e-12);
        mag_sq *= num / den;
    }
    10.0 * mag_sq.max(1e-8).log10()
}

#[derive(Clone, Default)]
struct BiquadState {
    z1_l: f32,
    z2_l: f32,
    z1_r: f32,
    z2_r: f32,
}

impl BiquadState {
    fn step(&mut self, c: &BiquadCoeffs, l: f32, r: f32) -> (f32, f32) {
        let out_l = c.b0 * l + self.z1_l;
        self.z1_l = c.b1 * l - c.a1 * out_l + self.z2_l;
        self.z2_l = c.b2 * l - c.a2 * out_l;

        let out_r = c.b0 * r + self.z1_r;
        self.z1_r = c.b1 * r - c.a1 * out_r + self.z2_r;
        self.z2_r = c.b2 * r - c.a2 * out_r;
        (out_l, out_r)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone)]
struct EqRt {
    states: [BiquadState; 7],
    coeffs: [BiquadCoeffs; 7],
    last_cfg: Option<EqCfg>,
    last_sr: f32,
}

impl Default for EqRt {
    fn default() -> Self {
        Self {
            states: std::array::from_fn(|_| BiquadState::default()),
            coeffs: [BiquadCoeffs::default(); 7],
            last_cfg: None,
            last_sr: 0.0,
        }
    }
}

impl EqRt {
    fn process(&mut self, cfg: &EqCfg, sr: f32, mut l: f32, mut r: f32) -> (f32, f32) {
        if self.last_cfg.as_ref() != Some(cfg) || (self.last_sr - sr).abs() > 1.0 {
            for (i, b) in cfg.bands.iter().enumerate() {
                self.coeffs[i] = design_biquad(b, sr);
            }
            self.last_cfg = Some(cfg.clone());
            self.last_sr = sr;
        }
        for (i, b) in cfg.bands.iter().enumerate() {
            if b.enabled {
                (l, r) = self.states[i].step(&self.coeffs[i], l, r);
            }
        }
        (l, r)
    }

    fn reset(&mut self) {
        for s in &mut self.states {
            s.reset();
        }
    }
}

// ---------------------------------------------------------------------------
// Compressor / Brickwall Limiter w/ sidechain + optional lookahead
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CompRt {
    env_db: f32,
    last_gr_db: f32,
    lookahead_buf: Vec<(f32, f32)>,
    la_pos: usize,
}

impl Default for CompRt {
    fn default() -> Self {
        Self {
            env_db: -96.0,
            last_gr_db: 0.0,
            lookahead_buf: vec![(0.0, 0.0); 257],
            la_pos: 0,
        }
    }
}

impl CompRt {
    fn process(
        &mut self,
        cfg: &CompCfg,
        sr: f32,
        in_l: f32,
        in_r: f32,
        sc_l: f32,
        sc_r: f32,
    ) -> (f32, f32) {
        let key_level = sc_l.abs().max(sc_r.abs());
        let key_db = gain_to_db(key_level);
        let atk = (-1.0 / ((cfg.attack_ms.max(0.1) * 0.001) * sr.max(1.0))).exp();
        let rel = (-1.0 / ((cfg.release_ms.max(5.0) * 0.001) * sr.max(1.0))).exp();

        let coeff = if key_db > self.env_db { atk } else { rel };
        self.env_db = key_db + coeff * (self.env_db - key_db);

        let thresh = cfg.threshold_db.clamp(-60.0, 0.0);
        let knee = cfg.knee_db.clamp(0.0, 24.0);
        let ratio = if cfg.limiter {
            100.0
        } else {
            cfg.ratio.clamp(1.0, 30.0)
        };
        let over = self.env_db - thresh;
        let gr_db = if knee > 0.0 && over > -knee * 0.5 && over < knee * 0.5 {
            let x = over + knee * 0.5;
            let slope = 1.0 - 1.0 / ratio;
            -slope * (x * x) / (2.0 * knee)
        } else if over > 0.0 {
            -over * (1.0 - 1.0 / ratio)
        } else {
            0.0
        };
        self.last_gr_db = gr_db;

        let (aud_l, aud_r) = if cfg.limiter && cfg.lookahead_samples > 0 {
            let la = cfg.lookahead_samples.min(self.lookahead_buf.len() - 1);
            let len = self.lookahead_buf.len();
            let rp = (self.la_pos + len - la) % len;
            let delayed = self.lookahead_buf[rp];
            self.lookahead_buf[self.la_pos] = (in_l, in_r);
            self.la_pos = (self.la_pos + 1) % len;
            delayed
        } else {
            (in_l, in_r)
        };

        let total_gain = db_to_gain(gr_db + cfg.makeup_db.clamp(-24.0, 24.0));
        let mut out_l = aud_l * total_gain;
        let mut out_r = aud_r * total_gain;
        if cfg.limiter {
            let ceil = db_to_gain(thresh.max(-24.0));
            out_l = out_l.clamp(-ceil, ceil);
            out_r = out_r.clamp(-ceil, ceil);
        }
        (out_l, out_r)
    }

    fn reset(&mut self) {
        self.env_db = -96.0;
        self.last_gr_db = 0.0;
        self.lookahead_buf.fill((0.0, 0.0));
        self.la_pos = 0;
    }
}

// ---------------------------------------------------------------------------
// Tempo-Synced Stereo Delay
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StereoDelayRt {
    buf: Vec<(f32, f32)>,
    pos: usize,
    lp_l: f32,
    lp_r: f32,
    hp_l: f32,
    hp_r: f32,
    hp_prev_l: f32,
    hp_prev_r: f32,
}

impl Default for StereoDelayRt {
    fn default() -> Self {
        Self::new(48000.0)
    }
}

impl StereoDelayRt {
    fn new(sr: f32) -> Self {
        let len = (sr.max(44100.0) * 2.0) as usize + 4;
        Self {
            buf: vec![(0.0, 0.0); len],
            pos: 0,
            lp_l: 0.0,
            lp_r: 0.0,
            hp_l: 0.0,
            hp_r: 0.0,
            hp_prev_l: 0.0,
            hp_prev_r: 0.0,
        }
    }

    fn process(&mut self, cfg: &DelayFxCfg, sr: f32, bpm: f32, in_l: f32, in_r: f32) -> (f32, f32) {
        let len = self.buf.len().max(1);
        let secs = cfg.sync.delay_secs(bpm, cfg.time_ms);
        let tap = ((secs * sr) as usize).clamp(1, len - 1);
        let rp = (self.pos + len - tap) % len;
        let (dl, dr) = self.buf[rp];

        let lp_a = (TAU * cfg.lp_hz.clamp(200.0, 18000.0) / sr.max(1.0)).clamp(0.01, 0.99);
        let hp_rc = 1.0 / (TAU * cfg.hp_hz.clamp(20.0, 4000.0)).max(1.0);
        let dt = 1.0 / sr.max(1.0);
        let hp_a = hp_rc / (hp_rc + dt);

        self.hp_l = hp_a * (self.hp_l + dl - self.hp_prev_l);
        self.hp_prev_l = dl;
        self.hp_r = hp_a * (self.hp_r + dr - self.hp_prev_r);
        self.hp_prev_r = dr;

        self.lp_l += lp_a * (self.hp_l - self.lp_l);
        self.lp_r += lp_a * (self.hp_r - self.lp_r);

        let fb = cfg.feedback.clamp(0.0, 0.92);
        let (fb_l, fb_r) = if cfg.ping_pong {
            (self.lp_r * fb, self.lp_l * fb)
        } else {
            (self.lp_l * fb, self.lp_r * fb)
        };
        let in_feed_l = if cfg.ping_pong {
            (in_l + in_r) * 0.5
        } else {
            in_l
        };
        let in_feed_r = if cfg.ping_pong { 0.0 } else { in_r };
        self.buf[self.pos] = (in_feed_l + fb_l, in_feed_r + fb_r);
        self.pos = (self.pos + 1) % len;
        (dl, dr)
    }

    fn reset(&mut self) {
        self.buf.fill((0.0, 0.0));
        self.pos = 0;
        self.lp_l = 0.0;
        self.lp_r = 0.0;
        self.hp_l = 0.0;
        self.hp_r = 0.0;
        self.hp_prev_l = 0.0;
        self.hp_prev_r = 0.0;
    }
}

// ---------------------------------------------------------------------------
// Algorithmic Reverb (Freeverb-style 8 combs + 4 allpass + predelay)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Comb {
    buf: Vec<f32>,
    pos: usize,
    damp_state: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len.max(16)],
            pos: 0,
            damp_state: 0.0,
        }
    }

    fn step(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.pos];
        self.damp_state = out * (1.0 - damp) + self.damp_state * damp;
        self.buf[self.pos] = input + self.damp_state * feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        out
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
        self.pos = 0;
        self.damp_state = 0.0;
    }
}

#[derive(Clone)]
struct Allpass {
    buf: Vec<f32>,
    pos: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self {
            buf: vec![0.0; len.max(16)],
            pos: 0,
        }
    }

    fn step(&mut self, input: f32) -> f32 {
        let buf_out = self.buf[self.pos];
        let out = -input + buf_out;
        self.buf[self.pos] = input + buf_out * 0.5;
        self.pos = (self.pos + 1) % self.buf.len();
        out
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
        self.pos = 0;
    }
}

#[derive(Clone)]
struct ReverbRt {
    pre: Vec<(f32, f32)>,
    pre_pos: usize,
    combs_l: [Comb; 4],
    combs_r: [Comb; 4],
    ap_l: [Allpass; 2],
    ap_r: [Allpass; 2],
    hc_l: f32,
    hc_r: f32,
}

impl Default for ReverbRt {
    fn default() -> Self {
        Self::new(48000.0)
    }
}

impl ReverbRt {
    fn new(sr: f32) -> Self {
        let scale = (sr / 44100.0).max(0.5);
        let mk_comb = |base: usize| Comb::new((base as f32 * scale) as usize);
        let mk_ap = |base: usize| Allpass::new((base as f32 * scale) as usize);
        Self {
            pre: vec![(0.0, 0.0); (sr.max(44100.0) * 0.12) as usize + 8],
            pre_pos: 0,
            combs_l: [mk_comb(1116), mk_comb(1277), mk_comb(1422), mk_comb(1557)],
            combs_r: [mk_comb(1139), mk_comb(1300), mk_comb(1445), mk_comb(1580)],
            ap_l: [mk_ap(556), mk_ap(441)],
            ap_r: [mk_ap(579), mk_ap(464)],
            hc_l: 0.0,
            hc_r: 0.0,
        }
    }

    fn process(&mut self, cfg: &ReverbFxCfg, sr: f32, in_l: f32, in_r: f32) -> (f32, f32) {
        let plen = self.pre.len().max(1);
        let pd_samples = ((cfg.pre_delay_ms.clamp(0.0, 100.0) * 0.001) * sr) as usize;
        let pd_samples = pd_samples.min(plen - 1);
        let rp = (self.pre_pos + plen - pd_samples) % plen;
        let (pl, pr) = self.pre[rp];
        self.pre[self.pre_pos] = (in_l, in_r);
        self.pre_pos = (self.pre_pos + 1) % plen;

        let mono = (pl + pr) * 0.25;
        let fb = (0.5 + cfg.decay.clamp(0.05, 0.96) * 0.44).clamp(0.1, 0.95)
            * (0.75 + cfg.room_size.clamp(0.1, 1.0) * 0.24);
        let damp = 1.0 - (cfg.high_cut_hz.clamp(600.0, 18000.0) / 20000.0).clamp(0.05, 0.9);

        let mut acc_l = 0.0f32;
        let mut acc_r = 0.0f32;
        for c in &mut self.combs_l {
            acc_l += c.step(mono, fb.min(0.95), damp);
        }
        for c in &mut self.combs_r {
            acc_r += c.step(mono, fb.min(0.95), damp);
        }
        for ap in &mut self.ap_l {
            acc_l = ap.step(acc_l);
        }
        for ap in &mut self.ap_r {
            acc_r = ap.step(acc_r);
        }

        let hc_a = (TAU * cfg.high_cut_hz.clamp(600.0, 18000.0) / sr.max(1.0)).clamp(0.02, 0.99);
        self.hc_l += hc_a * (acc_l - self.hc_l);
        self.hc_r += hc_a * (acc_r - self.hc_r);

        let mid = (self.hc_l + self.hc_r) * 0.5;
        let side = (self.hc_l - self.hc_r) * 0.5 * cfg.width.clamp(0.0, 1.5);
        (mid + side, mid - side)
    }

    fn reset(&mut self) {
        self.pre.fill((0.0, 0.0));
        self.pre_pos = 0;
        for c in &mut self.combs_l {
            c.reset();
        }
        for c in &mut self.combs_r {
            c.reset();
        }
        for a in &mut self.ap_l {
            a.reset();
        }
        for a in &mut self.ap_r {
            a.reset();
        }
        self.hc_l = 0.0;
        self.hc_r = 0.0;
    }
}

// ---------------------------------------------------------------------------
// Modulation (Chorus / Flanger / Phaser), Saturator, Stereo Shaper
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ModFxRt {
    buf: Vec<(f32, f32)>,
    pos: usize,
    phase: f32,
    ap_l: [f32; 4],
    ap_r: [f32; 4],
    fb_l: f32,
    fb_r: f32,
}

impl Default for ModFxRt {
    fn default() -> Self {
        Self {
            buf: vec![(0.0, 0.0); 4096],
            pos: 0,
            phase: 0.0,
            ap_l: [0.0; 4],
            ap_r: [0.0; 4],
            fb_l: 0.0,
            fb_r: 0.0,
        }
    }
}

impl ModFxRt {
    fn process(&mut self, cfg: &ModFxCfg, sr: f32, in_l: f32, in_r: f32) -> (f32, f32) {
        let rate = cfg.rate_hz.clamp(0.05, 10.0);
        self.phase = (self.phase + rate / sr.max(1.0)) % 1.0;
        let lfo_l = (self.phase * TAU).sin();
        let lfo_r = ((self.phase + 0.25) * TAU).sin();
        let depth = cfg.depth.clamp(0.0, 1.0);
        let fb = cfg.feedback.clamp(-0.85, 0.85);

        match cfg.mode {
            ModFxKind::Chorus | ModFxKind::Flanger => {
                let len = self.buf.len();
                let (base_ms, mod_ms) = if cfg.mode == ModFxKind::Chorus {
                    (12.0, 6.0 * depth)
                } else {
                    (2.0, 1.8 * depth)
                };
                let tap_l =
                    ((base_ms + mod_ms * lfo_l).max(0.2) * 0.001 * sr).min((len - 2) as f32);
                let tap_r =
                    ((base_ms + mod_ms * lfo_r).max(0.2) * 0.001 * sr).min((len - 2) as f32);

                let read = |buf: &[(f32, f32)], pos: usize, tap: f32, chan: usize| -> f32 {
                    let i0 = tap.floor() as usize;
                    let frac = tap - i0 as f32;
                    let p0 = (pos + len - i0) % len;
                    let p1 = (pos + len - i0 - 1) % len;
                    let s0 = if chan == 0 { buf[p0].0 } else { buf[p0].1 };
                    let s1 = if chan == 0 { buf[p1].0 } else { buf[p1].1 };
                    s0 + (s1 - s0) * frac
                };

                let dl = read(&self.buf, self.pos, tap_l, 0);
                let dr = read(&self.buf, self.pos, tap_r, 1);
                self.buf[self.pos] = (in_l + dl * fb, in_r + dr * fb);
                self.pos = (self.pos + 1) % len;
                ((in_l + dl) * 0.7, (in_r + dr) * 0.7)
            }
            ModFxKind::Phaser => {
                let sweep_l = 300.0 + (1800.0 * depth * (0.5 + 0.5 * lfo_l));
                let sweep_r = 300.0 + (1800.0 * depth * (0.5 + 0.5 * lfo_r));
                let coeff = |f: f32| {
                    let wc = (PI * f / sr.max(1.0)).min(1.2);
                    (1.0 - wc) / (1.0 + wc)
                };
                let kl = coeff(sweep_l);
                let kr = coeff(sweep_r);
                let mut xl = in_l + self.fb_l * fb;
                let mut xr = in_r + self.fb_r * fb;
                for st in &mut self.ap_l {
                    let y = kl * xl + *st;
                    *st = xl - kl * y;
                    xl = y;
                }
                for st in &mut self.ap_r {
                    let y = kr * xr + *st;
                    *st = xr - kr * y;
                    xr = y;
                }
                self.fb_l = xl;
                self.fb_r = xr;
                ((in_l + xl) * 0.65, (in_r + xr) * 0.65)
            }
        }
    }

    fn reset(&mut self) {
        self.buf.fill((0.0, 0.0));
        self.pos = 0;
        self.ap_l = [0.0; 4];
        self.ap_r = [0.0; 4];
        self.fb_l = 0.0;
        self.fb_r = 0.0;
    }
}

#[derive(Clone, Default)]
struct SaturatorRt {
    lp_l: f32,
    lp_r: f32,
}

impl SaturatorRt {
    fn process(&mut self, cfg: &SatCfg, sr: f32, in_l: f32, in_r: f32) -> (f32, f32) {
        let drive = cfg.drive.clamp(1.0, 16.0);
        let shape = |x: f32| -> f32 {
            let d = x * drive;
            match cfg.mode {
                SatKind::SoftClip => d.tanh(),
                SatKind::Tube => {
                    let bias = d + 0.12;
                    bias.tanh() - 0.12_f32.tanh()
                }
                SatKind::Wavefold => (d * 1.3).sin(),
            }
        };
        let sl = shape(in_l);
        let sr_v = shape(in_r);
        let cut = 1200.0 + cfg.tone.clamp(0.0, 1.0) * 16800.0;
        let alpha = (TAU * cut / sr.max(1.0)).clamp(0.05, 0.99);
        self.lp_l += alpha * (sl - self.lp_l);
        self.lp_r += alpha * (sr_v - self.lp_r);
        let out_g = db_to_gain(cfg.output_db.clamp(-24.0, 12.0));
        (self.lp_l * out_g, self.lp_r * out_g)
    }

    fn reset(&mut self) {
        self.lp_l = 0.0;
        self.lp_r = 0.0;
    }
}

#[derive(Clone)]
struct StereoShaperRt {
    buf: Vec<(f32, f32)>,
    pos: usize,
}

impl Default for StereoShaperRt {
    fn default() -> Self {
        Self {
            buf: vec![(0.0, 0.0); 2048],
            pos: 0,
        }
    }
}

impl StereoShaperRt {
    fn process(&mut self, cfg: &StereoShaperCfg, sr: f32, in_l: f32, in_r: f32) -> (f32, f32) {
        let len = self.buf.len();
        self.buf[self.pos] = (in_l, in_r);
        let d_smp = ((cfg.delay_ms.abs().min(20.0) * 0.001) * sr) as usize;
        let d_smp = d_smp.min(len - 1);
        let rp = (self.pos + len - d_smp) % len;
        let (mut l, mut r) = if cfg.delay_ms > 0.01 {
            (in_l, self.buf[rp].1)
        } else if cfg.delay_ms < -0.01 {
            (self.buf[rp].0, in_r)
        } else {
            (in_l, in_r)
        };
        self.pos = (self.pos + 1) % len;

        if cfg.invert_l {
            l = -l;
        }
        if cfg.invert_r {
            r = -r;
        }
        let mid = (l + r) * 0.5 * cfg.mid_gain.clamp(0.0, 2.0);
        let side = (l - r) * 0.5 * cfg.side_gain.clamp(0.0, 2.0);
        (mid + side, mid - side)
    }

    fn reset(&mut self) {
        self.buf.fill((0.0, 0.0));
        self.pos = 0;
    }
}

// ---------------------------------------------------------------------------
// Per-Insert Preallocated 10-Slot FX Rack Runtime
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct FxSlotRt {
    eq: EqRt,
    comp: CompRt,
    delay: StereoDelayRt,
    reverb: ReverbRt,
    mod_fx: ModFxRt,
    sat: SaturatorRt,
    shaper: StereoShaperRt,
    pub last_gr_db: f32,
}

impl FxSlotRt {
    pub fn new(sr: f32) -> Self {
        Self {
            eq: EqRt::default(),
            comp: CompRt::default(),
            delay: StereoDelayRt::new(sr),
            reverb: ReverbRt::new(sr),
            mod_fx: ModFxRt::default(),
            sat: SaturatorRt::default(),
            shaper: StereoShaperRt::default(),
            last_gr_db: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.eq.reset();
        self.comp.reset();
        self.delay.reset();
        self.reverb.reset();
        self.mod_fx.reset();
        self.sat.reset();
        self.shaper.reset();
        self.last_gr_db = 0.0;
    }

    pub fn process_sample(
        &mut self,
        slot: &crate::song::EffectSlot,
        sr: f32,
        bpm: f32,
        in_l: f32,
        in_r: f32,
        sc_bus: &[(f32, f32)],
    ) -> (f32, f32) {
        if slot.bypass || slot.mix <= 0.001 {
            self.last_gr_db = 0.0;
            return (in_l, in_r);
        }
        let (wet_l, wet_r) = match &slot.kind {
            FxKind::ParametricEq(cfg) => self.eq.process(cfg, sr, in_l, in_r),
            FxKind::Compressor(cfg) => {
                let (sc_l, sc_r) = cfg
                    .sidechain_from
                    .and_then(|idx| sc_bus.get(idx).copied())
                    .unwrap_or((in_l, in_r));
                let out = self.comp.process(cfg, sr, in_l, in_r, sc_l, sc_r);
                self.last_gr_db = self.comp.last_gr_db;
                out
            }
            FxKind::StereoDelay(cfg) => self.delay.process(cfg, sr, bpm, in_l, in_r),
            FxKind::Reverb(cfg) => self.reverb.process(cfg, sr, in_l, in_r),
            FxKind::Modulator(cfg) => self.mod_fx.process(cfg, sr, in_l, in_r),
            FxKind::Saturator(cfg) => self.sat.process(cfg, sr, in_l, in_r),
            FxKind::StereoShaper(cfg) => self.shaper.process(cfg, sr, in_l, in_r),
        };
        let mix = slot.mix.clamp(0.0, 1.0);
        (in_l + (wet_l - in_l) * mix, in_r + (wet_r - in_r) * mix)
    }
}

#[derive(Clone)]
pub struct MixerInsertRt {
    pub slots: Vec<FxSlotRt>,
    pub rms_sq: f32,
    pub clip_hold: usize,
    pub spec_ring: [f32; FFT_N],
    pub spec_pos: usize,
}

impl MixerInsertRt {
    pub fn new(sr: f32) -> Self {
        Self {
            slots: (0..MAX_FX_SLOTS).map(|_| FxSlotRt::new(sr)).collect(),
            rms_sq: 0.0,
            clip_hold: 0,
            spec_ring: [0.0; FFT_N],
            spec_pos: 0,
        }
    }

    pub fn reset(&mut self) {
        for s in &mut self.slots {
            s.reset();
        }
        self.rms_sq = 0.0;
        self.clip_hold = 0;
        self.spec_ring.fill(0.0);
        self.spec_pos = 0;
    }

    pub fn process_insert_fx(
        &mut self,
        track: &MixerTrack,
        sr: f32,
        bpm: f32,
        mut l: f32,
        mut r: f32,
        sc_bus: &[(f32, f32)],
    ) -> (f32, f32) {
        for (slot, rt) in track.fx.iter().zip(self.slots.iter_mut()) {
            (l, r) = rt.process_sample(slot, sr, bpm, l, r, sc_bus);
        }
        if track.polarity_inv {
            l = -l;
            r = -r;
        }
        if track.stereo_sep.abs() > 1e-4 {
            let mid = (l + r) * 0.5;
            let side = (l - r) * 0.5 * (1.0 + track.stereo_sep.clamp(-1.0, 1.0));
            l = mid + side;
            r = mid - side;
        }
        self.spec_ring[self.spec_pos] = (l + r) * 0.5;
        self.spec_pos = (self.spec_pos + 1) % FFT_N;
        (l, r)
    }

    pub fn update_meters(&mut self, l: f32, r: f32) -> f32 {
        let e = 0.5 * (l * l + r * r);
        self.rms_sq += 0.002 * (e - self.rms_sq);
        if l.abs() >= 1.0 || r.abs() >= 1.0 {
            self.clip_hold = 22050;
        } else if self.clip_hold > 0 {
            self.clip_hold -= 1;
        }
        self.rms_sq.max(0.0).sqrt()
    }
}

// ---------------------------------------------------------------------------
// Real-time 256-point Radix-2 Cooley-Tukey FFT for Parametric EQ Analyzer
// ---------------------------------------------------------------------------

pub fn compute_fft_bins(ring: &[f32; FFT_N], ring_pos: usize, out_db: &mut [f32; FFT_BINS]) {
    let mut re = [0.0f32; FFT_N];
    let mut im = [0.0f32; FFT_N];
    for (k, slot) in re.iter_mut().enumerate() {
        let s = ring[(ring_pos + k) % FFT_N];
        let win = 0.5 * (1.0 - (TAU * k as f32 / (FFT_N - 1) as f32).cos());
        *slot = s * win;
    }

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..FFT_N {
        let mut bit = FFT_N >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Iterative Cooley-Tukey stages.
    let mut len = 2usize;
    while len <= FFT_N {
        let ang = -TAU / len as f32;
        let wlen_re = ang.cos();
        let wlen_im = ang.sin();
        let half = len / 2;
        for start in (0..FFT_N).step_by(len) {
            let mut w_re = 1.0f32;
            let mut w_im = 0.0f32;
            for k in 0..half {
                let u_re = re[start + k];
                let u_im = im[start + k];
                let vr = re[start + k + half];
                let vi = im[start + k + half];
                let t_re = vr * w_re - vi * w_im;
                let t_im = vr * w_im + vi * w_re;
                re[start + k] = u_re + t_re;
                im[start + k] = u_im + t_im;
                re[start + k + half] = u_re - t_re;
                im[start + k + half] = u_im - t_im;
                let nw_re = w_re * wlen_re - w_im * wlen_im;
                let nw_im = w_re * wlen_im + w_im * wlen_re;
                w_re = nw_re;
                w_im = nw_im;
            }
        }
        len <<= 1;
    }

    // Map 128 positive bins logarithmically across 64 display bars (20 Hz..Nyquist).
    let half_n = FFT_N / 2;
    for (b, slot) in out_db.iter_mut().enumerate() {
        let f0 = (b as f32 / FFT_BINS as f32).powf(2.0);
        let f1 = ((b + 1) as f32 / FFT_BINS as f32).powf(2.0);
        let i0 = ((f0 * (half_n - 1) as f32).floor() as usize).clamp(1, half_n - 1);
        let i1 = ((f1 * (half_n - 1) as f32).ceil() as usize).clamp(i0 + 1, half_n);
        let mut max_mag = 0.0f32;
        for idx in i0..i1 {
            let mag = (re[idx] * re[idx] + im[idx] * im[idx]).sqrt() / (FFT_N as f32 * 0.25);
            max_mag = max_mag.max(mag);
        }
        *slot = gain_to_db(max_mag).clamp(-72.0, 6.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::song::EffectSlot;

    #[test]
    fn db_gain_roundtrip() {
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-5);
        assert!((db_to_gain(6.0) - 1.99526).abs() < 1e-3);
        assert!(db_to_gain(-80.0) == 0.0);
        assert!((gain_to_db(db_to_gain(-12.0)) + 12.0).abs() < 1e-3);
    }

    #[test]
    fn parametric_eq_boosts_target_band_and_attenuates_hpf() {
        let mut eq = EqCfg::default();
        eq.bands[0].enabled = true;
        eq.bands[0].freq_hz = 200.0;
        eq.bands[3].freq_hz = 1000.0;
        eq.bands[3].gain_db = 6.0;
        assert!(eq_response_db(&eq, 1000.0, 44100.0) > 4.5);
        assert!(eq_response_db(&eq, 40.0, 44100.0) < -12.0);

        let mut rt = EqRt::default();
        let mut out_1k = 0.0f32;
        for i in 0..512 {
            let s = (TAU * 1000.0 * i as f32 / 44100.0).sin() * 0.25;
            let (l, _) = rt.process(&eq, 44100.0, s, s);
            if i > 256 {
                out_1k = out_1k.max(l.abs());
            }
        }
        assert!(
            out_1k > 0.35,
            "1kHz boost should raise amplitude, got {out_1k}"
        );
    }

    #[test]
    fn compressor_and_limiter_reduce_excess_gain() {
        let mut comp = CompRt::default();
        let cfg = CompCfg {
            threshold_db: -18.0,
            ratio: 8.0,
            knee_db: 0.0,
            attack_ms: 0.5,
            release_ms: 50.0,
            makeup_db: 0.0,
            sidechain_from: None,
            limiter: false,
            lookahead_samples: 0,
        };
        let mut out_peak = 0.0f32;
        for i in 0..1024 {
            let (l, _) = comp.process(&cfg, 44100.0, 0.8, 0.8, 0.8, 0.8);
            if i > 512 {
                out_peak = out_peak.max(l.abs());
            }
        }
        assert!(
            out_peak < 0.4,
            "compressor should attenuate 0.8 signal: {out_peak}"
        );
        assert!(comp.last_gr_db < -6.0);

        // Brickwall limiter clamps output at ceiling.
        let lim = CompCfg {
            threshold_db: -6.0,
            limiter: true,
            lookahead_samples: 16,
            ..cfg
        };
        for _ in 0..64 {
            let (l, r) = comp.process(&lim, 44100.0, 1.5, -1.5, 1.5, -1.5);
            assert!(l.abs() <= db_to_gain(-6.0) + 1e-4);
            assert!(r.abs() <= db_to_gain(-6.0) + 1e-4);
        }
    }

    #[test]
    fn delay_reverb_mod_sat_shaper_are_finite_and_audible() {
        let sr = 44100.0;
        let slots = [
            EffectSlot::new(FxKind::StereoDelay(DelayFxCfg::default())),
            EffectSlot::new(FxKind::Reverb(ReverbFxCfg::default())),
            EffectSlot::new(FxKind::Modulator(ModFxCfg {
                mode: ModFxKind::Phaser,
                ..Default::default()
            })),
            EffectSlot::new(FxKind::Saturator(SatCfg {
                mode: SatKind::Tube,
                ..Default::default()
            })),
            EffectSlot::new(FxKind::StereoShaper(StereoShaperCfg {
                delay_ms: 4.0,
                ..Default::default()
            })),
        ];
        for slot in &slots {
            let mut rt = FxSlotRt::new(sr);
            let mut energy = 0.0f32;
            for i in 0..22050 {
                let x = if i < 64 { 0.5 } else { 0.0 };
                let (l, r) = rt.process_sample(slot, sr, 128.0, x, x, &[]);
                assert!(l.is_finite() && r.is_finite());
                energy += l * l + r * r;
            }
            assert!(energy > 0.01, "{} produced too little energy", slot.name);
        }
    }

    #[test]
    fn fft_detects_sine_peak() {
        let mut ring = [0.0f32; FFT_N];
        let freq = 2756.25; // bin 16 at 44100 Hz
        for (i, slot) in ring.iter_mut().enumerate() {
            *slot = (TAU * freq * i as f32 / 44100.0).sin();
        }
        let mut bins = [-96.0f32; FFT_BINS];
        compute_fft_bins(&ring, 0, &mut bins);
        let peak_bin = bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap();
        assert!(
            peak_bin > 10 && peak_bin < 40,
            "unexpected peak bin {peak_bin}"
        );
        assert!(bins[peak_bin] > -10.0);
    }
}
