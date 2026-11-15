//! Advanced built-in synthesizers: SuperSynth (3 oscs, unison + detune +
//! stereo spread, sub/noise, sync/ring mod, multimode resonant SVF + drive +
//! keytrack, 2 LFOs) and 4-operator FM Synth, plus factory & JSON presets.

use crate::song::{
    Channel, ChannelKind, ChannelPreset, FilterMode, FmAlgo, FmOpCfg, FmSynthCfg, LfoCfg,
    ModTarget, OscCfg, SuperSynthCfg, WaveKind,
};
use std::f32::consts::{PI, TAU};

const MAX_UNISON: usize = 7;

#[derive(Clone)]
struct EnvState {
    level: f32,
    stage: u8, // 0=idle, 1=attack, 2=decay, 3=sustain, 4=release
}

impl Default for EnvState {
    fn default() -> Self {
        Self {
            level: 0.0,
            stage: 0,
        }
    }
}

impl EnvState {
    fn trigger(&mut self) {
        self.stage = 1;
    }

    fn release(&mut self) {
        if self.stage != 0 {
            self.stage = 4;
        }
    }

    fn kill(&mut self) {
        self.level = 0.0;
        self.stage = 0;
    }

    fn step(&mut self, sr: f32, a: f32, d: f32, s: f32, r: f32, rel_mul: f32) -> f32 {
        let s = s.clamp(0.0, 1.0);
        match self.stage {
            1 => {
                let inc = 1.0 / (a.max(0.001) * sr.max(1.0));
                self.level += inc;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = 2;
                }
            }
            2 => {
                let dec = (1.0 - s) / (d.max(0.002) * sr.max(1.0));
                self.level -= dec;
                if self.level <= s {
                    self.level = s;
                    self.stage = 3;
                }
            }
            3 => {
                self.level = s;
            }
            4 => {
                let rel = self.level.max(0.05) / ((r.max(0.003) * rel_mul.max(0.1)) * sr.max(1.0));
                self.level -= rel;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.stage = 0;
                }
            }
            _ => {
                self.level = 0.0;
            }
        }
        self.level
    }
}

#[derive(Clone)]
pub struct AdvVoiceRt {
    pub active: bool,
    pub off: usize,
    pub age: u64,
    pub cur_hz: f32,
    pub target_hz: f32,
    pub glide_rate: f32,
    pub slide_rem: usize,
    pub slide_tot: usize,
    pub slide_start_hz: f32,
    pub vel: f32,
    pub pan: f32,
    pub slide_vel_start: f32,
    pub slide_vel_target: f32,
    pub slide_pan_start: f32,
    pub slide_pan_target: f32,
    pub cutoff_mul: f32,
    pub rel_mul: f32,
    osc_phases: [[f32; 3]; MAX_UNISON],
    sub_phase: f32,
    lfo_phases: [f32; 2],
    fm_phases: [f32; 4],
    fm_prev: [f32; 4],
    note_time: f32,
    noise_seed: u64,
    env: EnvState,
    svf_ic1_l: f32,
    svf_ic2_l: f32,
    svf_ic1_r: f32,
    svf_ic2_r: f32,
}

impl Default for AdvVoiceRt {
    fn default() -> Self {
        Self {
            active: false,
            off: 0,
            age: 0,
            cur_hz: 440.0,
            target_hz: 440.0,
            glide_rate: 1.0,
            slide_rem: 0,
            slide_tot: 0,
            slide_start_hz: 440.0,
            vel: 1.0,
            pan: 0.0,
            slide_vel_start: 1.0,
            slide_vel_target: 1.0,
            slide_pan_start: 0.0,
            slide_pan_target: 0.0,
            cutoff_mul: 1.0,
            rel_mul: 1.0,
            osc_phases: [[0.0; 3]; MAX_UNISON],
            sub_phase: 0.0,
            lfo_phases: [0.0; 2],
            fm_phases: [0.0; 4],
            fm_prev: [0.0; 4],
            note_time: 0.0,
            noise_seed: 0x5eed_1234_9abc_def0,
            env: EnvState::default(),
            svf_ic1_l: 0.0,
            svf_ic2_l: 0.0,
            svf_ic1_r: 0.0,
            svf_ic2_r: 0.0,
        }
    }
}

#[inline]
fn eval_wave(wave: WaveKind, phase: f32, noise_seed: &mut u64) -> f32 {
    let p = phase.rem_euclid(1.0);
    match wave {
        WaveKind::Sine => (p * TAU).sin(),
        WaveKind::Triangle => 1.0 - 4.0 * (p - 0.5).abs(),
        WaveKind::Saw => 2.0 * p - 1.0,
        WaveKind::Square => {
            if p < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        WaveKind::Noise => {
            *noise_seed = noise_seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(37);
            ((*noise_seed >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0
        }
    }
}

impl AdvVoiceRt {
    pub fn trigger(
        &mut self,
        start_hz: f32,
        target_hz: f32,
        glide: f32,
        vel: f32,
        pan: f32,
        off: usize,
    ) {
        self.active = true;
        self.cur_hz = start_hz;
        self.target_hz = target_hz;
        self.glide_rate = glide;
        self.slide_rem = 0;
        self.slide_tot = 0;
        self.slide_start_hz = start_hz;
        self.vel = vel.clamp(0.05, 1.0);
        self.pan = pan.clamp(-1.0, 1.0);
        self.slide_vel_start = self.vel;
        self.slide_vel_target = self.vel;
        self.slide_pan_start = self.pan;
        self.slide_pan_target = self.pan;
        self.off = off;
        self.note_time = 0.0;
        self.fm_phases = [0.0; 4];
        self.fm_prev = [0.0; 4];
        self.env.trigger();
    }

    pub fn kill(&mut self) {
        self.active = false;
        self.off = 0;
        self.slide_rem = 0;
        self.env.kill();
    }

    pub fn tick_gate(&mut self) {
        if self.slide_rem > 0 && self.slide_tot > 0 {
            self.slide_rem -= 1;
            let frac = 1.0 - (self.slide_rem as f32 / self.slide_tot as f32);
            let start = self.slide_start_hz.max(20.0);
            self.cur_hz = (start * (self.target_hz / start).powf(frac)).clamp(20.0, 20000.0);
            self.vel = self.slide_vel_start + (self.slide_vel_target - self.slide_vel_start) * frac;
            self.pan = self.slide_pan_start + (self.slide_pan_target - self.slide_pan_start) * frac;
        } else if (self.cur_hz - self.target_hz).abs() > 0.01 {
            self.cur_hz += (self.target_hz - self.cur_hz) * self.glide_rate;
        }
        if self.off > 0 {
            self.off -= 1;
            if self.off == 0 {
                self.env.release();
            }
        }
    }

    pub fn render_super(
        &mut self,
        ch: &Channel,
        cfg: &SuperSynthCfg,
        sr: f32,
        base_cut: f32,
    ) -> (f32, f32) {
        if !self.active && self.env.stage == 0 {
            return (0.0, 0.0);
        }
        let env_g = self.env.step(
            sr,
            ch.attack,
            ch.decay,
            ch.sustain,
            ch.release,
            self.rel_mul,
        );
        if self.env.stage == 0 {
            self.active = false;
            return (0.0, 0.0);
        }

        // Evaluate 2 LFOs.
        let mut pitch_mod_semi = 0.0f32;
        let mut cut_mod_oct = 0.0f32;
        let mut amp_mod = 1.0f32;
        let mut pan_mod = 0.0f32;
        for (li, lfo) in cfg.lfos.iter().enumerate() {
            self.lfo_phases[li] =
                (self.lfo_phases[li] + lfo.rate_hz.clamp(0.05, 30.0) / sr.max(1.0)) % 1.0;
            if lfo.depth.abs() <= 1e-4 || lfo.target == ModTarget::None {
                continue;
            }
            let val = eval_wave(lfo.wave, self.lfo_phases[li], &mut self.noise_seed) * lfo.depth;
            match lfo.target {
                ModTarget::None => {}
                ModTarget::Pitch => pitch_mod_semi += val * 2.0,
                ModTarget::Cutoff => cut_mod_oct += val * 2.5,
                ModTarget::Amp => amp_mod *= (1.0 + val * 0.5).clamp(0.1, 1.5),
                ModTarget::Pan => pan_mod += val * 0.7,
            }
        }

        let hz = (self.cur_hz * 2.0_f32.powf(pitch_mod_semi / 12.0)).clamp(20.0, sr * 0.45);
        let uni = (cfg.unison as usize).clamp(1, MAX_UNISON);
        let detune_max_cents = cfg.detune.clamp(0.0, 1.0) * 35.0;
        let spread = cfg.spread.clamp(0.0, 1.0);
        let mut sum_l = 0.0f32;
        let mut sum_r = 0.0f32;

        for u in 0..uni {
            let norm = if uni == 1 {
                0.0
            } else {
                (u as f32 / (uni - 1) as f32) * 2.0 - 1.0
            };
            let detune_ratio = 2.0_f32.powf((norm * detune_max_cents) / 1200.0);
            let mut osc_vals = [0.0f32; 3];
            let mut wrapped0 = false;
            for (oi, osc_out) in osc_vals.iter_mut().enumerate() {
                let oc = &cfg.oscs[oi];
                let o_ratio = 2.0_f32.powf((oc.semi as f32 * 100.0 + oc.cents) / 1200.0);
                let inc = (hz * detune_ratio * o_ratio / sr.max(1.0)).clamp(0.0, 0.48);
                let prev_p = self.osc_phases[u][oi];
                let mut next_p = prev_p + inc;
                if next_p >= 1.0 {
                    next_p -= next_p.floor();
                    if oi == 0 {
                        wrapped0 = true;
                    }
                }
                if oi == 1 && cfg.sync_osc2 && wrapped0 {
                    next_p = 0.0;
                }
                self.osc_phases[u][oi] = next_p;
                *osc_out = eval_wave(oc.wave, next_p, &mut self.noise_seed) * oc.level;
            }
            let voice_s = if cfg.ring_mod {
                osc_vals[0] * (1.0 + osc_vals[1]) + osc_vals[2]
            } else {
                osc_vals[0] + osc_vals[1] + osc_vals[2]
            };
            let pan_u = (norm * spread).clamp(-1.0, 1.0);
            let angle = (pan_u + 1.0) * std::f32::consts::FRAC_PI_4;
            sum_l += voice_s * angle.cos();
            sum_r += voice_s * angle.sin();
        }
        let inv_uni = 1.0 / (uni as f32).sqrt();
        sum_l *= inv_uni;
        sum_r *= inv_uni;

        // Sub oscillator (-1 oct) + Noise.
        if cfg.sub_level > 1e-4 {
            self.sub_phase = (self.sub_phase + (hz * 0.5) / sr.max(1.0)) % 1.0;
            let sub = (self.sub_phase * TAU).sin() * cfg.sub_level;
            sum_l += sub;
            sum_r += sub;
        }
        if cfg.noise_level > 1e-4 {
            let nz = eval_wave(WaveKind::Noise, 0.0, &mut self.noise_seed) * cfg.noise_level;
            sum_l += nz;
            sum_r += nz;
        }

        // Pre-filter drive.
        if cfg.drive > 0.01 {
            let d = 1.0 + cfg.drive.clamp(0.0, 1.0) * 5.0;
            sum_l = (sum_l * d).tanh();
            sum_r = (sum_r * d).tanh();
        }

        // Multimode Cytomic TPT SVF with keytrack & filter envelope.
        let key_oct = ((hz / 261.63).max(0.05).log2()) * cfg.keytrack.clamp(0.0, 1.0);
        let env_oct = env_g * cfg.filter_env.clamp(-1.0, 1.0) * 3.0;
        let cut_hz = (base_cut * self.cutoff_mul * 2.0_f32.powf(cut_mod_oct + key_oct + env_oct))
            .clamp(40.0, (sr * 0.45).max(80.0));
        let g = (PI * cut_hz / sr.max(1.0)).tan();
        let k = 1.0 / ch.q.clamp(0.3, 8.0);
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;

        let svf = |x: f32, ic1: &mut f32, ic2: &mut f32| -> f32 {
            let v3 = x - *ic2;
            let v1 = a1 * *ic1 + a2 * v3;
            let v2 = *ic2 + a2 * *ic1 + a3 * v3;
            *ic1 = 2.0 * v1 - *ic1;
            *ic2 = 2.0 * v2 - *ic2;
            match cfg.filter_mode {
                FilterMode::Lp => v2,
                FilterMode::Bp => v1,
                FilterMode::Hp => x - k * v1 - v2,
                FilterMode::Notch => x - k * v1,
            }
        };
        let fl = svf(sum_l, &mut self.svf_ic1_l, &mut self.svf_ic2_l);
        let fr = svf(sum_r, &mut self.svf_ic1_r, &mut self.svf_ic2_r);

        let pan_total = (self.pan + pan_mod).clamp(-1.0, 1.0);
        let angle = (pan_total + 1.0) * std::f32::consts::FRAC_PI_4;
        let g_amp = env_g * self.vel * amp_mod * 0.45;
        (fl * angle.cos() * g_amp, fr * angle.sin() * g_amp)
    }

    pub fn render_fm(
        &mut self,
        ch: &Channel,
        cfg: &FmSynthCfg,
        sr: f32,
        base_cut: f32,
    ) -> (f32, f32) {
        if !self.active && self.env.stage == 0 {
            return (0.0, 0.0);
        }
        let env_g = self.env.step(
            sr,
            ch.attack,
            ch.decay,
            ch.sustain,
            ch.release,
            self.rel_mul,
        );
        if self.env.stage == 0 {
            self.active = false;
            return (0.0, 0.0);
        }
        self.note_time += 1.0 / sr.max(1.0);
        let hz = self.cur_hz.clamp(20.0, sr * 0.45);
        for (oi, op) in cfg.ops.iter().enumerate() {
            let inc = (hz * op.ratio.clamp(0.125, 16.0) / sr.max(1.0)).clamp(0.0, 0.48);
            self.fm_phases[oi] = (self.fm_phases[oi] + inc) % 1.0;
        }
        let op_env = |oi: usize| -> f32 {
            let dec = cfg.ops[oi].decay.clamp(0.02, 8.0);
            (-self.note_time / dec).exp() * cfg.ops[oi].level.clamp(0.0, 2.0)
        };
        let op_eval = |oi: usize, phase: f32, prev: f32, mod_in: f32| -> f32 {
            let fb = prev * cfg.ops[oi].feedback.clamp(0.0, 1.5);
            (phase * TAU + mod_in + fb).sin() * op_env(oi)
        };

        let p = self.fm_phases;
        let prev = self.fm_prev;
        let out = match cfg.algo {
            FmAlgo::Serial => {
                let o3 = op_eval(3, p[3], prev[3], 0.0);
                let o2 = op_eval(2, p[2], prev[2], o3 * 3.0);
                let o1 = op_eval(1, p[1], prev[1], o2 * 3.0);
                let o0 = op_eval(0, p[0], prev[0], o1 * 3.0);
                self.fm_prev = [o0, o1, o2, o3];
                o0
            }
            FmAlgo::Parallel => {
                let o3 = op_eval(3, p[3], prev[3], 0.0);
                let o2 = op_eval(2, p[2], prev[2], o3 * 3.0);
                let o1 = op_eval(1, p[1], prev[1], 0.0);
                let o0 = op_eval(0, p[0], prev[0], o1 * 3.0);
                self.fm_prev = [o0, o1, o2, o3];
                (o0 + o2) * 0.65
            }
            FmAlgo::PairStack => {
                let o3 = op_eval(3, p[3], prev[3], 0.0);
                let o2 = op_eval(2, p[2], prev[2], 0.0);
                let o1 = op_eval(1, p[1], prev[1], (o2 + o3) * 2.2);
                let o0 = op_eval(0, p[0], prev[0], o1 * 3.0);
                self.fm_prev = [o0, o1, o2, o3];
                o0
            }
            FmAlgo::YBranch => {
                let o3 = op_eval(3, p[3], prev[3], 0.0);
                let o2 = op_eval(2, p[2], prev[2], 0.0);
                let o1 = op_eval(1, p[1], prev[1], o2 * 2.5);
                let o0 = op_eval(0, p[0], prev[0], (o1 + o3) * 2.5);
                self.fm_prev = [o0, o1, o2, o3];
                o0
            }
        };

        // Light tone LP matching channel cutoff.
        let cut_hz = (base_cut * self.cutoff_mul).clamp(80.0, (sr * 0.45).max(160.0));
        let alpha = (TAU * cut_hz / sr.max(1.0)).clamp(0.02, 0.99);
        self.svf_ic1_l += alpha * (out - self.svf_ic1_l);
        let filtered = self.svf_ic1_l;

        let angle = (self.pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
        let g = env_g * self.vel * 0.45;
        (filtered * angle.cos() * g, filtered * angle.sin() * g)
    }
}

pub fn factory_presets() -> Vec<ChannelPreset> {
    vec![
        ChannelPreset {
            name: "Supersaw Lead".into(),
            kind: ChannelKind::SuperSynth(SuperSynthCfg {
                unison: 5,
                detune: 0.38,
                spread: 0.8,
                sub_level: 0.25,
                drive: 0.15,
                ..Default::default()
            }),
            cutoff: 4800.0,
            q: 1.2,
            attack: 0.008,
            decay: 0.2,
            sustain: 0.75,
            release: 0.18,
        },
        ChannelPreset {
            name: "Reese Bass".into(),
            kind: ChannelKind::SuperSynth(SuperSynthCfg {
                oscs: [
                    OscCfg {
                        wave: WaveKind::Saw,
                        semi: 0,
                        cents: 0.0,
                        level: 0.9,
                    },
                    OscCfg {
                        wave: WaveKind::Saw,
                        semi: 0,
                        cents: 14.0,
                        level: 0.9,
                    },
                    OscCfg {
                        wave: WaveKind::Square,
                        semi: -12,
                        cents: -6.0,
                        level: 0.5,
                    },
                ],
                unison: 4,
                detune: 0.45,
                spread: 0.45,
                sub_level: 0.5,
                noise_level: 0.0,
                sync_osc2: false,
                ring_mod: false,
                filter_mode: FilterMode::Lp,
                drive: 0.35,
                keytrack: 0.25,
                filter_env: 0.2,
                lfos: [
                    LfoCfg {
                        wave: WaveKind::Sine,
                        rate_hz: 1.5,
                        depth: 0.2,
                        target: ModTarget::Cutoff,
                    },
                    LfoCfg {
                        wave: WaveKind::Sine,
                        rate_hz: 4.0,
                        depth: 0.0,
                        target: ModTarget::None,
                    },
                ],
            }),
            cutoff: 650.0,
            q: 2.2,
            attack: 0.015,
            decay: 0.25,
            sustain: 0.85,
            release: 0.12,
        },
        ChannelPreset {
            name: "Lush Pad".into(),
            kind: ChannelKind::SuperSynth(SuperSynthCfg {
                unison: 7,
                detune: 0.32,
                spread: 0.95,
                sub_level: 0.1,
                noise_level: 0.04,
                filter_mode: FilterMode::Lp,
                filter_env: 0.25,
                ..Default::default()
            }),
            cutoff: 1600.0,
            q: 0.9,
            attack: 0.28,
            decay: 0.4,
            sustain: 0.8,
            release: 0.55,
        },
        ChannelPreset {
            name: "FM Bell".into(),
            kind: ChannelKind::FmSynth(FmSynthCfg {
                algo: FmAlgo::Parallel,
                ops: [
                    FmOpCfg {
                        ratio: 1.0,
                        level: 1.0,
                        decay: 1.8,
                        feedback: 0.0,
                    },
                    FmOpCfg {
                        ratio: 3.5,
                        level: 0.8,
                        decay: 0.55,
                        feedback: 0.0,
                    },
                    FmOpCfg {
                        ratio: 2.0,
                        level: 0.7,
                        decay: 1.2,
                        feedback: 0.1,
                    },
                    FmOpCfg {
                        ratio: 5.0,
                        level: 0.45,
                        decay: 0.22,
                        feedback: 0.2,
                    },
                ],
            }),
            cutoff: 11000.0,
            q: 0.8,
            attack: 0.002,
            decay: 0.5,
            sustain: 0.2,
            release: 0.45,
        },
        ChannelPreset {
            name: "Solid FM Bass".into(),
            kind: ChannelKind::FmSynth(FmSynthCfg {
                algo: FmAlgo::Serial,
                ops: [
                    FmOpCfg {
                        ratio: 1.0,
                        level: 1.0,
                        decay: 0.8,
                        feedback: 0.0,
                    },
                    FmOpCfg {
                        ratio: 1.0,
                        level: 0.75,
                        decay: 0.28,
                        feedback: 0.25,
                    },
                    FmOpCfg {
                        ratio: 2.0,
                        level: 0.5,
                        decay: 0.15,
                        feedback: 0.1,
                    },
                    FmOpCfg {
                        ratio: 4.0,
                        level: 0.25,
                        decay: 0.08,
                        feedback: 0.0,
                    },
                ],
            }),
            cutoff: 2800.0,
            q: 1.0,
            attack: 0.002,
            decay: 0.22,
            sustain: 0.65,
            release: 0.08,
        },
    ]
}

pub fn apply_preset_to_channel(ch: &mut Channel, preset: &ChannelPreset) {
    ch.name = preset.name.clone();
    ch.kind = preset.kind.clone();
    ch.cutoff = preset.cutoff;
    ch.q = preset.q;
    ch.attack = preset.attack;
    ch.decay = preset.decay;
    ch.sustain = preset.sustain;
    ch.release = preset.release;
}

pub fn save_preset_json(ch: &Channel, path: &str) -> Result<(), String> {
    let preset = ChannelPreset {
        name: ch.name.clone(),
        kind: ch.kind.clone(),
        cutoff: ch.cutoff,
        q: ch.q,
        attack: ch.attack,
        decay: ch.decay,
        sustain: ch.sustain,
        release: ch.release,
    };
    let json = serde_json::to_string_pretty(&preset).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

pub fn load_preset_json(path: &str) -> Result<ChannelPreset, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supersynth_unison_renders_stereo_and_finite() {
        let mut ch = Channel::default();
        let cfg = SuperSynthCfg {
            unison: 5,
            detune: 0.4,
            spread: 0.9,
            ..Default::default()
        };
        ch.kind = ChannelKind::SuperSynth(cfg.clone());
        let mut v = AdvVoiceRt::default();
        v.trigger(220.0, 220.0, 1.0, 0.9, 0.0, 2000);
        let mut diff = 0.0f32;
        let mut peak = 0.0f32;
        for _ in 0..4000 {
            v.tick_gate();
            let (l, r) = v.render_super(&ch, &cfg, 44100.0, ch.cutoff);
            assert!(l.is_finite() && r.is_finite());
            diff += (l - r).abs();
            peak = peak.max(l.abs().max(r.abs()));
        }
        assert!(peak > 0.05, "SuperSynth silent: {peak}");
        assert!(
            diff > 1.0,
            "SuperSynth unison spread should produce stereo difference"
        );
    }

    #[test]
    fn fm_synth_and_presets_roundtrip() {
        let presets = factory_presets();
        assert!(presets.len() >= 5);
        let mut ch = Channel::default();
        apply_preset_to_channel(&mut ch, &presets[3]);
        let ChannelKind::FmSynth(cfg) = &ch.kind else {
            panic!("expected FmSynth preset");
        };
        let mut v = AdvVoiceRt::default();
        v.trigger(440.0, 440.0, 1.0, 1.0, 0.0, 1000);
        let mut peak = 0.0f32;
        for _ in 0..2000 {
            v.tick_gate();
            let (l, r) = v.render_fm(&ch, cfg, 44100.0, ch.cutoff);
            assert!(l.is_finite() && r.is_finite());
            peak = peak.max(l.abs().max(r.abs()));
        }
        assert!(peak > 0.05);

        let path = std::env::temp_dir().join("thunder-preset-test.json");
        let path_s = path.to_str().unwrap();
        save_preset_json(&ch, path_s).unwrap();
        let back = load_preset_json(path_s).unwrap();
        assert_eq!(back.name, ch.name);
        std::fs::remove_file(path_s).ok();
    }
}
