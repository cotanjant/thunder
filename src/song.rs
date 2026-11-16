//! Song model: FL-style dynamic channels, variable-length patterns with
//! time signatures, multi-track playlist clips, routable mixer insert DAG
//! with PDC calculation, and versioned JSON save/load (v1 -> v2 migration).

use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const TICKS_PER_STEP: u32 = 4; // 64th-note resolution
pub const TICKS_PATTERN: u32 = 64; // one 4/4 bar
pub const N_PATTERNS: usize = 8;
pub const DEFAULT_MIXER_INSERTS: usize = 16;
pub const SAVE_VERSION: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaveKind {
    Sine,
    Triangle,
    Saw,
    Square,
    Noise,
}

impl WaveKind {
    pub fn all() -> [WaveKind; 5] {
        use WaveKind::*;
        [Sine, Triangle, Saw, Square, Noise]
    }
    pub fn name(&self) -> &'static str {
        match self {
            WaveKind::Sine => "Sine",
            WaveKind::Triangle => "Tri",
            WaveKind::Saw => "Saw",
            WaveKind::Square => "Square",
            WaveKind::Noise => "Noise",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KitPiece {
    Kick,
    Snare,
    Clap,
    Hat,
}

impl KitPiece {
    pub fn all() -> [KitPiece; 4] {
        use KitPiece::*;
        [Kick, Snare, Clap, Hat]
    }
    pub fn name(&self) -> &'static str {
        match self {
            KitPiece::Kick => "Kick",
            KitPiece::Snare => "Snare",
            KitPiece::Clap => "Clap",
            KitPiece::Hat => "Hat",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StretchMode {
    Resample,
    Granular,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SamplerCfg {
    pub trim_start: f32,
    pub trim_end: f32,
    pub loop_enabled: bool,
    pub loop_start: f32,
    pub loop_end: f32,
    pub loop_xfade: f32,
    pub reverse: bool,
    pub stretch: StretchMode,
    pub filter_env_amt: f32,
}

impl Default for SamplerCfg {
    fn default() -> Self {
        Self {
            trim_start: 0.0,
            trim_end: 1.0,
            loop_enabled: false,
            loop_start: 0.25,
            loop_end: 0.75,
            loop_xfade: 0.05,
            reverse: false,
            stretch: StretchMode::Resample,
            filter_env_amt: 0.0,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PadLayer {
    pub vel_min: f32,
    pub vel_max: f32,
    pub sample: Option<usize>,
    pub kit_fallback: Option<KitPiece>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DrumPad {
    pub name: String,
    pub note: u8,
    pub gain: f32,
    pub pan: f32,
    pub tune_semi: f32,
    pub cut_group: Option<u8>,
    pub cut_by: Option<u8>,
    pub mixer_offset: i8,
    pub layers: Vec<PadLayer>,
}

type PadSpec = (
    &'static str,
    u8,
    Option<KitPiece>,
    f32,
    Option<u8>,
    Option<u8>,
);

pub fn default_drum_pads() -> Vec<DrumPad> {
    let specs: [PadSpec; 16] = [
        ("Kick", 36, Some(KitPiece::Kick), 0.0, None, None),
        ("Rim", 37, Some(KitPiece::Snare), 7.0, None, None),
        ("Snare", 38, Some(KitPiece::Snare), 0.0, None, None),
        ("Clap", 39, Some(KitPiece::Clap), 0.0, None, None),
        ("ClosedHat", 40, Some(KitPiece::Hat), 0.0, None, Some(1)),
        ("LowTom", 41, Some(KitPiece::Kick), 5.0, None, None),
        ("PedalHat", 42, Some(KitPiece::Hat), -2.0, Some(1), Some(1)),
        ("MidTom", 43, Some(KitPiece::Kick), 9.0, None, None),
        ("OpenHat", 44, Some(KitPiece::Hat), -5.0, Some(1), Some(1)),
        ("HighTom", 45, Some(KitPiece::Kick), 12.0, None, None),
        ("Crash", 46, Some(KitPiece::Hat), -7.0, None, None),
        ("Ride", 47, Some(KitPiece::Hat), 3.0, None, None),
        ("Perc1", 48, Some(KitPiece::Clap), 4.0, None, None),
        ("Perc2", 49, Some(KitPiece::Clap), -4.0, None, None),
        ("Perc3", 50, Some(KitPiece::Snare), -6.0, None, None),
        ("Perc4", 51, Some(KitPiece::Snare), 10.0, None, None),
    ];
    specs
        .into_iter()
        .map(|(name, note, kit, tune, cg, cb)| DrumPad {
            name: name.into(),
            note,
            gain: 1.0,
            pan: 0.0,
            tune_semi: tune,
            cut_group: cg,
            cut_by: cb,
            mixer_offset: 0,
            layers: vec![PadLayer {
                vel_min: 0.0,
                vel_max: 1.0,
                sample: None,
                kit_fallback: kit,
            }],
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterMode {
    Lp,
    Bp,
    Hp,
    Notch,
}

impl FilterMode {
    pub const ALL: [FilterMode; 4] = [
        FilterMode::Lp,
        FilterMode::Bp,
        FilterMode::Hp,
        FilterMode::Notch,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            FilterMode::Lp => "LP",
            FilterMode::Bp => "BP",
            FilterMode::Hp => "HP",
            FilterMode::Notch => "Notch",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModTarget {
    None,
    Pitch,
    Cutoff,
    Amp,
    Pan,
}

impl ModTarget {
    pub const ALL: [ModTarget; 5] = [
        ModTarget::None,
        ModTarget::Pitch,
        ModTarget::Cutoff,
        ModTarget::Amp,
        ModTarget::Pan,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            ModTarget::None => "Off",
            ModTarget::Pitch => "Pitch",
            ModTarget::Cutoff => "Cutoff",
            ModTarget::Amp => "Amp",
            ModTarget::Pan => "Pan",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OscCfg {
    pub wave: WaveKind,
    pub semi: i8,
    pub cents: f32,
    pub level: f32,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LfoCfg {
    pub wave: WaveKind,
    pub rate_hz: f32,
    pub depth: f32,
    pub target: ModTarget,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SuperSynthCfg {
    pub oscs: [OscCfg; 3],
    pub unison: u8,
    pub detune: f32,
    pub spread: f32,
    pub sub_level: f32,
    pub noise_level: f32,
    pub sync_osc2: bool,
    pub ring_mod: bool,
    pub filter_mode: FilterMode,
    pub drive: f32,
    pub keytrack: f32,
    pub filter_env: f32,
    pub lfos: [LfoCfg; 2],
}

impl Default for SuperSynthCfg {
    fn default() -> Self {
        Self {
            oscs: [
                OscCfg {
                    wave: WaveKind::Saw,
                    semi: 0,
                    cents: 0.0,
                    level: 0.85,
                },
                OscCfg {
                    wave: WaveKind::Saw,
                    semi: 0,
                    cents: 7.0,
                    level: 0.6,
                },
                OscCfg {
                    wave: WaveKind::Square,
                    semi: -12,
                    cents: -5.0,
                    level: 0.4,
                },
            ],
            unison: 3,
            detune: 0.25,
            spread: 0.65,
            sub_level: 0.3,
            noise_level: 0.0,
            sync_osc2: false,
            ring_mod: false,
            filter_mode: FilterMode::Lp,
            drive: 0.1,
            keytrack: 0.35,
            filter_env: 0.35,
            lfos: [
                LfoCfg {
                    wave: WaveKind::Sine,
                    rate_hz: 2.5,
                    depth: 0.15,
                    target: ModTarget::Cutoff,
                },
                LfoCfg {
                    wave: WaveKind::Triangle,
                    rate_hz: 5.0,
                    depth: 0.0,
                    target: ModTarget::Pitch,
                },
            ],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FmAlgo {
    Serial,
    Parallel,
    PairStack,
    YBranch,
}

impl FmAlgo {
    pub const ALL: [FmAlgo; 4] = [
        FmAlgo::Serial,
        FmAlgo::Parallel,
        FmAlgo::PairStack,
        FmAlgo::YBranch,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            FmAlgo::Serial => "4->3->2->1",
            FmAlgo::Parallel => "(4->3)+(2->1)",
            FmAlgo::PairStack => "(4+3)->2->1",
            FmAlgo::YBranch => "(4+3->2)->1",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FmOpCfg {
    pub ratio: f32,
    pub level: f32,
    pub decay: f32,
    pub feedback: f32,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct FmSynthCfg {
    pub algo: FmAlgo,
    pub ops: [FmOpCfg; 4],
}

impl Default for FmSynthCfg {
    fn default() -> Self {
        Self {
            algo: FmAlgo::Parallel,
            ops: [
                FmOpCfg {
                    ratio: 1.0,
                    level: 1.0,
                    decay: 1.2,
                    feedback: 0.0,
                },
                FmOpCfg {
                    ratio: 2.0,
                    level: 0.65,
                    decay: 0.45,
                    feedback: 0.0,
                },
                FmOpCfg {
                    ratio: 1.0,
                    level: 0.55,
                    decay: 0.85,
                    feedback: 0.15,
                },
                FmOpCfg {
                    ratio: 3.5,
                    level: 0.4,
                    decay: 0.25,
                    feedback: 0.2,
                },
            ],
        }
    }
}

static CLAP_RT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_clap_rt_id() -> u64 {
    CLAP_RT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClapPluginCfg {
    #[serde(skip, default = "next_clap_rt_id")]
    pub rt_id: u64,
    pub path: String,
    pub plugin_id: String,
    pub name: String,
    #[serde(default)]
    pub state_b64: String,
    #[serde(default)]
    pub params: Vec<(u32, f64)>,
    #[serde(default)]
    pub reported_latency: usize,
}

impl ClapPluginCfg {
    pub fn new(
        path: impl Into<String>,
        plugin_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            rt_id: next_clap_rt_id(),
            path: path.into(),
            plugin_id: plugin_id.into(),
            name: name.into(),
            state_b64: String::new(),
            params: Vec::new(),
            reported_latency: 0,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum ChannelKind {
    Synth { wave: WaveKind },
    Kit { piece: KitPiece },
    Sampler { sample: Option<usize> },
    SuperSynth(SuperSynthCfg),
    FmSynth(FmSynthCfg),
    DrumRack { pads: Vec<DrumPad> },
    ClapPlugin(ClapPluginCfg),
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelPreset {
    pub name: String,
    pub kind: ChannelKind,
    pub cutoff: f32,
    pub q: f32,
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

pub const MAX_FX_SLOTS: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EqBandType {
    HighPass,
    LowShelf,
    Bell,
    HighShelf,
    LowPass,
}

impl EqBandType {
    pub const ALL: [EqBandType; 5] = [
        EqBandType::HighPass,
        EqBandType::LowShelf,
        EqBandType::Bell,
        EqBandType::HighShelf,
        EqBandType::LowPass,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            EqBandType::HighPass => "HPF",
            EqBandType::LowShelf => "LShelf",
            EqBandType::Bell => "Bell",
            EqBandType::HighShelf => "HShelf",
            EqBandType::LowPass => "LPF",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub enabled: bool,
    pub kind: EqBandType,
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct EqCfg {
    pub bands: [EqBand; 7],
}

impl Default for EqCfg {
    fn default() -> Self {
        Self {
            bands: [
                EqBand {
                    enabled: false,
                    kind: EqBandType::HighPass,
                    freq_hz: 35.0,
                    gain_db: 0.0,
                    q: 0.71,
                },
                EqBand {
                    enabled: true,
                    kind: EqBandType::LowShelf,
                    freq_hz: 110.0,
                    gain_db: 0.0,
                    q: 0.71,
                },
                EqBand {
                    enabled: true,
                    kind: EqBandType::Bell,
                    freq_hz: 280.0,
                    gain_db: 0.0,
                    q: 1.0,
                },
                EqBand {
                    enabled: true,
                    kind: EqBandType::Bell,
                    freq_hz: 1000.0,
                    gain_db: 0.0,
                    q: 1.0,
                },
                EqBand {
                    enabled: true,
                    kind: EqBandType::Bell,
                    freq_hz: 3500.0,
                    gain_db: 0.0,
                    q: 1.0,
                },
                EqBand {
                    enabled: true,
                    kind: EqBandType::HighShelf,
                    freq_hz: 8500.0,
                    gain_db: 0.0,
                    q: 0.71,
                },
                EqBand {
                    enabled: false,
                    kind: EqBandType::LowPass,
                    freq_hz: 16500.0,
                    gain_db: 0.0,
                    q: 0.71,
                },
            ],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompCfg {
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    pub sidechain_from: Option<usize>,
    pub limiter: bool,
    pub lookahead_samples: usize,
}

impl Default for CompCfg {
    fn default() -> Self {
        Self {
            threshold_db: -14.0,
            ratio: 4.0,
            knee_db: 4.0,
            attack_ms: 8.0,
            release_ms: 90.0,
            makeup_db: 0.0,
            sidechain_from: None,
            limiter: false,
            lookahead_samples: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelaySync {
    FreeMs,
    Sync1_4,
    Sync1_8,
    Sync1_16,
    Sync1_8Dot,
    Sync1_8Trip,
}

impl DelaySync {
    pub const ALL: [DelaySync; 6] = [
        DelaySync::FreeMs,
        DelaySync::Sync1_4,
        DelaySync::Sync1_8,
        DelaySync::Sync1_16,
        DelaySync::Sync1_8Dot,
        DelaySync::Sync1_8Trip,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            DelaySync::FreeMs => "Free ms",
            DelaySync::Sync1_4 => "1/4",
            DelaySync::Sync1_8 => "1/8",
            DelaySync::Sync1_16 => "1/16",
            DelaySync::Sync1_8Dot => "1/8.",
            DelaySync::Sync1_8Trip => "1/8T",
        }
    }
    pub fn delay_secs(&self, bpm: f32, free_ms: f32) -> f32 {
        let beat = 60.0 / bpm.max(30.0);
        match self {
            DelaySync::FreeMs => (free_ms * 0.001).clamp(0.005, 1.9),
            DelaySync::Sync1_4 => beat.clamp(0.005, 1.9),
            DelaySync::Sync1_8 => (beat * 0.5).clamp(0.005, 1.9),
            DelaySync::Sync1_16 => (beat * 0.25).clamp(0.005, 1.9),
            DelaySync::Sync1_8Dot => (beat * 0.75).clamp(0.005, 1.9),
            DelaySync::Sync1_8Trip => (beat / 3.0).clamp(0.005, 1.9),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DelayFxCfg {
    pub sync: DelaySync,
    pub time_ms: f32,
    pub feedback: f32,
    pub ping_pong: bool,
    pub hp_hz: f32,
    pub lp_hz: f32,
}

impl Default for DelayFxCfg {
    fn default() -> Self {
        Self {
            sync: DelaySync::Sync1_8,
            time_ms: 350.0,
            feedback: 0.42,
            ping_pong: true,
            hp_hz: 120.0,
            lp_hz: 6500.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReverbFxCfg {
    pub room_size: f32,
    pub decay: f32,
    pub pre_delay_ms: f32,
    pub high_cut_hz: f32,
    pub width: f32,
}

impl Default for ReverbFxCfg {
    fn default() -> Self {
        Self {
            room_size: 0.6,
            decay: 0.65,
            pre_delay_ms: 18.0,
            high_cut_hz: 8000.0,
            width: 0.85,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModFxKind {
    Chorus,
    Flanger,
    Phaser,
}

impl ModFxKind {
    pub const ALL: [ModFxKind; 3] = [ModFxKind::Chorus, ModFxKind::Flanger, ModFxKind::Phaser];
    pub fn name(&self) -> &'static str {
        match self {
            ModFxKind::Chorus => "Chorus",
            ModFxKind::Flanger => "Flanger",
            ModFxKind::Phaser => "Phaser",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModFxCfg {
    pub mode: ModFxKind,
    pub rate_hz: f32,
    pub depth: f32,
    pub feedback: f32,
}

impl Default for ModFxCfg {
    fn default() -> Self {
        Self {
            mode: ModFxKind::Chorus,
            rate_hz: 0.8,
            depth: 0.5,
            feedback: 0.25,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SatKind {
    SoftClip,
    Tube,
    Wavefold,
}

impl SatKind {
    pub const ALL: [SatKind; 3] = [SatKind::SoftClip, SatKind::Tube, SatKind::Wavefold];
    pub fn name(&self) -> &'static str {
        match self {
            SatKind::SoftClip => "SoftClip",
            SatKind::Tube => "Tube",
            SatKind::Wavefold => "Wavefold",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SatCfg {
    pub mode: SatKind,
    pub drive: f32,
    pub tone: f32,
    pub output_db: f32,
}

impl Default for SatCfg {
    fn default() -> Self {
        Self {
            mode: SatKind::SoftClip,
            drive: 2.5,
            tone: 0.75,
            output_db: -2.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StereoShaperCfg {
    pub mid_gain: f32,
    pub side_gain: f32,
    pub delay_ms: f32,
    pub invert_l: bool,
    pub invert_r: bool,
}

impl Default for StereoShaperCfg {
    fn default() -> Self {
        Self {
            mid_gain: 1.0,
            side_gain: 1.0,
            delay_ms: 0.0,
            invert_l: false,
            invert_r: false,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum FxKind {
    ParametricEq(EqCfg),
    Compressor(CompCfg),
    StereoDelay(DelayFxCfg),
    Reverb(ReverbFxCfg),
    Modulator(ModFxCfg),
    Saturator(SatCfg),
    StereoShaper(StereoShaperCfg),
    ClapPlugin(ClapPluginCfg),
}

impl FxKind {
    pub fn default_name(&self) -> &'static str {
        match self {
            FxKind::ParametricEq(_) => "Parametric EQ",
            FxKind::Compressor(c) => {
                if c.limiter {
                    "Limiter"
                } else {
                    "Compressor"
                }
            }
            FxKind::StereoDelay(_) => "Stereo Delay",
            FxKind::Reverb(_) => "Reverb",
            FxKind::Modulator(m) => m.mode.name(),
            FxKind::Saturator(_) => "Saturator",
            FxKind::StereoShaper(_) => "Stereo Shaper",
            FxKind::ClapPlugin(_) => "CLAP Plugin",
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectSlot {
    pub name: String,
    pub bypass: bool,
    pub mix: f32,
    pub kind: FxKind,
}

impl EffectSlot {
    pub fn new(kind: FxKind) -> Self {
        let name = match &kind {
            FxKind::ClapPlugin(c) if !c.name.is_empty() => c.name.clone(),
            _ => kind.default_name().into(),
        };
        Self {
            name,
            bypass: false,
            mix: 1.0,
            kind,
        }
    }

    pub fn latency_samples(&self) -> usize {
        if self.bypass {
            return 0;
        }
        match &self.kind {
            FxKind::Compressor(c) if c.limiter => c.lookahead_samples.min(256),
            FxKind::ClapPlugin(c) => c.reported_latency,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TimeSig {
    pub num: u8,
    pub den: u8,
}

impl Default for TimeSig {
    fn default() -> Self {
        Self { num: 4, den: 4 }
    }
}

impl TimeSig {
    pub fn ticks_per_bar(&self) -> u32 {
        let num = (self.num as u32).clamp(1, 16);
        let den = match self.den {
            2 | 4 | 8 | 16 => self.den as u32,
            _ => 4,
        };
        ((num * TICKS_PER_STEP * 16) / den).max(TICKS_PER_STEP * 4)
    }

    pub fn ticks_per_beat(&self) -> u32 {
        let den = match self.den {
            2 | 4 | 8 | 16 => self.den as u32,
            _ => 4,
        };
        (TICKS_PER_STEP * 16 / den).max(TICKS_PER_STEP)
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.num, self.den)
    }
}

pub const SAMPLE_PEAK_BINS: usize = 256;

pub fn compute_sample_peaks(l: &[f32], r: &[f32]) -> Arc<[f32]> {
    if l.is_empty() {
        return Arc::from([]);
    }
    let bins = SAMPLE_PEAK_BINS.min(l.len()).max(1);
    let mut out = vec![0.0f32; bins];
    let step = l.len() as f32 / bins as f32;
    for (b, slot) in out.iter_mut().enumerate() {
        let start = (b as f32 * step) as usize;
        let end = (((b + 1) as f32 * step).ceil() as usize).clamp(start + 1, l.len());
        let mut peak = 0.0f32;
        for (i, &lv) in l.iter().enumerate().take(end).skip(start) {
            let vl = lv.abs();
            let vr = r.get(i).copied().unwrap_or(0.0).abs();
            peak = peak.max(vl.max(vr));
        }
        *slot = peak;
    }
    Arc::from(out)
}

/// One user sample. PCM is reloaded from `path` on project load
/// (never stored in the JSON).
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SampleRef {
    pub name: String,
    pub path: String,
    #[serde(skip)]
    pub data: Arc<[f32]>,
    #[serde(skip)]
    pub data_r: Arc<[f32]>,
    #[serde(skip)]
    pub peaks: Arc<[f32]>,
    #[serde(skip)]
    pub sr: f32,
}

impl SampleRef {
    pub fn from_mono(
        name: impl Into<String>,
        path: impl Into<String>,
        data: Vec<f32>,
        sr: f32,
    ) -> Self {
        let peaks = compute_sample_peaks(&data, &[]);
        Self {
            name: name.into(),
            path: path.into(),
            data: Arc::from(data),
            data_r: Arc::from([]),
            peaks,
            sr,
        }
    }

    pub fn from_stereo(
        name: impl Into<String>,
        path: impl Into<String>,
        left: Vec<f32>,
        right: Vec<f32>,
        sr: f32,
    ) -> Self {
        let peaks = compute_sample_peaks(&left, &right);
        Self {
            name: name.into(),
            path: path.into(),
            data: Arc::from(left),
            data_r: Arc::from(right),
            peaks,
            sr,
        }
    }

    pub fn is_stereo(&self) -> bool {
        !self.data_r.is_empty() && self.data_r.len() == self.data.len()
    }

    pub fn normalize_in_place(&mut self) {
        let peak = self
            .data
            .iter()
            .chain(self.data_r.iter())
            .map(|x| x.abs())
            .fold(0.0f32, f32::max);
        if peak < 1e-6 {
            return;
        }
        let g = 0.98 / peak;
        let l: Vec<f32> = self.data.iter().map(|x| x * g).collect();
        let r: Vec<f32> = self.data_r.iter().map(|x| x * g).collect();
        self.peaks = compute_sample_peaks(&l, &r);
        self.data = Arc::from(l);
        self.data_r = Arc::from(r);
    }

    pub fn remove_dc_in_place(&mut self) {
        if self.data.is_empty() {
            return;
        }
        let mean_l = self.data.iter().sum::<f32>() / self.data.len() as f32;
        let l: Vec<f32> = self.data.iter().map(|x| x - mean_l).collect();
        let r: Vec<f32> = if self.is_stereo() {
            let mean_r = self.data_r.iter().sum::<f32>() / self.data_r.len() as f32;
            self.data_r.iter().map(|x| x - mean_r).collect()
        } else {
            Vec::new()
        };
        self.peaks = compute_sample_peaks(&l, &r);
        self.data = Arc::from(l);
        self.data_r = Arc::from(r);
    }

    pub fn reverse_in_place(&mut self) {
        let mut l: Vec<f32> = self.data.iter().copied().collect();
        l.reverse();
        let mut r: Vec<f32> = self.data_r.iter().copied().collect();
        r.reverse();
        self.peaks = compute_sample_peaks(&l, &r);
        self.data = Arc::from(l);
        self.data_r = Arc::from(r);
    }

    pub fn trim_range_in_place(&mut self, start_frac: f32, end_frac: f32) {
        let len = self.data.len();
        if len < 2 {
            return;
        }
        let s = ((start_frac.clamp(0.0, 0.99) * len as f32) as usize).min(len - 2);
        let e = ((end_frac.clamp(0.01, 1.0) * len as f32) as usize).clamp(s + 1, len);
        let l: Vec<f32> = self.data[s..e].to_vec();
        let r: Vec<f32> = if self.is_stereo() {
            self.data_r[s..e].to_vec()
        } else {
            Vec::new()
        };
        self.peaks = compute_sample_peaks(&l, &r);
        self.data = Arc::from(l);
        self.data_r = Arc::from(r);
    }

    pub fn fade_in_range_in_place(&mut self, start_frac: f32, end_frac: f32) {
        let len = self.data.len();
        if len < 2 {
            return;
        }
        let s = ((start_frac.clamp(0.0, 0.99) * len as f32) as usize).min(len - 2);
        let e = ((end_frac.clamp(0.01, 1.0) * len as f32) as usize).clamp(s + 2, len);
        let span = (e - s) as f32;
        let mut l: Vec<f32> = self.data.to_vec();
        let mut r: Vec<f32> = self.data_r.to_vec();
        for i in s..e {
            let g = (i - s) as f32 / span;
            l[i] *= g;
            if i < r.len() {
                r[i] *= g;
            }
        }
        self.peaks = compute_sample_peaks(&l, &r);
        self.data = Arc::from(l);
        self.data_r = Arc::from(r);
    }

    pub fn fade_out_range_in_place(&mut self, start_frac: f32, end_frac: f32) {
        let len = self.data.len();
        if len < 2 {
            return;
        }
        let s = ((start_frac.clamp(0.0, 0.99) * len as f32) as usize).min(len - 2);
        let e = ((end_frac.clamp(0.01, 1.0) * len as f32) as usize).clamp(s + 2, len);
        let span = (e - s) as f32;
        let mut l: Vec<f32> = self.data.to_vec();
        let mut r: Vec<f32> = self.data_r.to_vec();
        for i in s..e {
            let g = 1.0 - (i - s) as f32 / span;
            l[i] *= g;
            if i < r.len() {
                r[i] *= g;
            }
        }
        self.peaks = compute_sample_peaks(&l, &r);
        self.data = Arc::from(l);
        self.data_r = Arc::from(r);
    }

    pub fn detect_transients(&self, threshold: f32, min_gap_samples: usize) -> Vec<usize> {
        let len = self.data.len();
        if len < 64 {
            return vec![0];
        }
        let gap = min_gap_samples.max(256);
        let mut marks = vec![0usize];
        let mut env_fast = 0.0f32;
        let mut env_slow = 0.0f32;
        let thresh = threshold.clamp(0.02, 0.9);
        for (i, &lv) in self.data.iter().enumerate() {
            let rv = self.data_r.get(i).copied().unwrap_or(0.0);
            let mag = lv.abs().max(rv.abs());
            env_fast += 0.08 * (mag - env_fast);
            env_slow += 0.002 * (mag - env_slow);
            if i > gap
                && (i - *marks.last().unwrap_or(&0)) >= gap
                && env_fast > thresh
                && env_fast > env_slow * 1.85
            {
                marks.push(i);
            }
        }
        marks
    }

    pub fn extract_slice(&self, name: impl Into<String>, start_smp: usize, end_smp: usize) -> Self {
        let len = self.data.len();
        if len == 0 {
            return Self::from_mono(name, "", Vec::new(), self.sr.max(44100.0));
        }
        let s = start_smp.min(len - 1);
        let e = end_smp.clamp(s + 1, len);
        if self.is_stereo() {
            Self::from_stereo(
                name,
                self.path.clone(),
                self.data[s..e].to_vec(),
                self.data_r[s..e].to_vec(),
                self.sr,
            )
        } else {
            Self::from_mono(name, self.path.clone(), self.data[s..e].to_vec(), self.sr)
        }
    }
}

impl PartialEq for SampleRef {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.path == other.path
            && self.sr == other.sr
            && self.data.len() == other.data.len()
            && self.data_r.len() == other.data_r.len()
    }
}

fn default_note_mid() -> f32 {
    0.5
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Note {
    pub tick: u32,
    pub len: u32,
    pub midi: u8,
    pub vel: f32,
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub fine_pitch: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub slide: bool,
    #[serde(default = "default_note_mid")]
    pub cutoff: f32,
    #[serde(default = "default_note_mid")]
    pub release_bias: f32,
}

impl Note {
    pub fn new(tick: u32, len: u32, midi: u8, vel: f32) -> Self {
        Self {
            tick,
            len: len.max(1),
            midi: midi.clamp(21, 108),
            vel: vel.clamp(0.05, 1.0),
            pan: 0.0,
            fine_pitch: 0.0,
            mute: false,
            slide: false,
            cutoff: 0.5,
            release_bias: 0.5,
        }
    }

    pub fn new_in(tick: u32, len: u32, midi: u8, vel: f32, max_ticks: u32) -> Self {
        Self::new(tick.min(max_ticks.saturating_sub(1)), len, midi, vel)
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AutoPoint {
    pub tick: u32,
    pub value: f32,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub name: String,
    pub kind: ChannelKind,
    #[serde(default = "default_ch_color")]
    pub color: [u8; 3],
    pub midi_base: u8,
    pub volume: f32,
    pub pan: f32,
    pub cutoff: f32,
    pub q: f32,
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
    pub mute: bool,
    pub solo: bool,
    pub delay_send: f32,
    pub reverb_send: f32,
    #[serde(default)]
    pub mixer_insert: usize,
    #[serde(default)]
    pub polyphony: u8,
    #[serde(default)]
    pub portamento: f32,
    #[serde(default)]
    pub cut_group: Option<u8>,
    #[serde(default)]
    pub cut_by: Option<u8>,
    #[serde(default)]
    pub sampler: SamplerCfg,
}

fn default_ch_color() -> [u8; 3] {
    [90, 140, 200]
}

impl Default for ChannelKind {
    fn default() -> Self {
        ChannelKind::Synth {
            wave: WaveKind::Saw,
        }
    }
}

/// Everything needed to build one fundsp voice, copied out of a [`Channel`].
/// Keeps voice construction honest instead of passing 6 loose DSP arguments.
#[derive(Clone, Copy, PartialEq)]
pub struct VoiceSpec {
    pub wave: WaveKind,
    pub q: f32,
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

impl Channel {
    pub fn voice_spec(&self) -> VoiceSpec {
        VoiceSpec {
            wave: match self.kind {
                ChannelKind::Synth { wave } => wave,
                // Kit/sampler/custom synth channels still keep a fallback voice spec.
                _ => WaveKind::Sine,
            },
            q: self.q,
            attack: self.attack,
            decay: self.decay,
            sustain: self.sustain,
            release: self.release,
        }
    }
}

impl Default for Channel {
    /// Sane DSP defaults: a zeroed filter (cutoff 0 / Q 0) NaNs fundsp's
    /// SVF and poisons the whole mix bus, so defaults must be playable.
    fn default() -> Self {
        Self {
            name: "Channel".into(),
            kind: ChannelKind::default(),
            color: default_ch_color(),
            midi_base: 60,
            volume: 0.8,
            pan: 0.0,
            cutoff: 2000.0,
            q: 1.0,
            attack: 0.005,
            decay: 0.1,
            sustain: 0.8,
            release: 0.1,
            mute: false,
            solo: false,
            delay_send: 0.0,
            reverb_send: 0.15,
            mixer_insert: 0,
            polyphony: 0,
            portamento: 0.0,
            cut_group: None,
            cut_by: None,
            sampler: SamplerCfg::default(),
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
    #[serde(default = "default_pat_name")]
    pub name: String,
    #[serde(default = "default_pat_color")]
    pub color: [u8; 3],
    #[serde(default = "default_bars")]
    pub bars: u16,
    #[serde(default)]
    pub time_sig: TimeSig,
    /// One note list per channel (channel indices align with Song::channels).
    pub notes: Vec<Vec<Note>>,
    pub auto_vol: Vec<Vec<AutoPoint>>,
    pub auto_cut: Vec<Vec<AutoPoint>>,
}

fn default_pat_name() -> String {
    "Pattern".into()
}

fn default_pat_color() -> [u8; 3] {
    [100, 175, 110]
}

fn default_bars() -> u16 {
    1
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            name: default_pat_name(),
            color: default_pat_color(),
            bars: 1,
            time_sig: TimeSig::default(),
            notes: Vec::new(),
            auto_vol: Vec::new(),
            auto_cut: Vec::new(),
        }
    }
}

impl Pattern {
    pub fn empty_for_channels(name: String, color: [u8; 3], nch: usize, time_sig: TimeSig) -> Self {
        Self {
            name,
            color,
            bars: 1,
            time_sig,
            notes: vec![Vec::new(); nch],
            auto_vol: vec![Vec::new(); nch],
            auto_cut: vec![Vec::new(); nch],
        }
    }

    pub fn len_ticks(&self) -> u32 {
        (self.bars.max(1) as u32) * self.time_sig.ticks_per_bar()
    }

    pub fn steps(&self) -> usize {
        (self.len_ticks() / TICKS_PER_STEP).max(1) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.notes.iter().all(Vec::is_empty)
            && self.auto_vol.iter().all(Vec::is_empty)
            && self.auto_cut.iter().all(Vec::is_empty)
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistTrack {
    pub name: String,
    pub color: [u8; 3],
    pub mute: bool,
    pub solo: bool,
    #[serde(default)]
    pub linked_channel: Option<usize>,
    #[serde(default)]
    pub linked_insert: Option<usize>,
}

impl PlaylistTrack {
    pub fn new(idx: usize) -> Self {
        Self {
            name: format!("Track {}", idx + 1),
            color: palette_color(idx),
            mute: false,
            solo: false,
            linked_channel: None,
            linked_insert: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoTarget {
    ChannelVol(usize),
    ChannelPan(usize),
    ChannelCut(usize),
    InsertVol(usize),
    InsertPan(usize),
    InsertFxMix(usize, usize),
    MasterVol,
}

impl AutoTarget {
    pub fn label(&self) -> String {
        match self {
            AutoTarget::ChannelVol(ch) => format!("Ch {} Vol", ch + 1),
            AutoTarget::ChannelPan(ch) => format!("Ch {} Pan", ch + 1),
            AutoTarget::ChannelCut(ch) => format!("Ch {} Cut", ch + 1),
            AutoTarget::InsertVol(ins) => {
                if *ins == 0 {
                    "Master Vol".into()
                } else {
                    format!("Ins {ins} Vol")
                }
            }
            AutoTarget::InsertPan(ins) => format!("Ins {ins} Pan"),
            AutoTarget::InsertFxMix(ins, slot) => format!("Ins {ins} FX{} Mix", slot + 1),
            AutoTarget::MasterVol => "Master Vol".into(),
        }
    }

    pub fn range(&self) -> (f32, f32) {
        match self {
            AutoTarget::ChannelVol(_) | AutoTarget::InsertVol(_) | AutoTarget::MasterVol => {
                (0.0, 1.5)
            }
            AutoTarget::ChannelPan(_) | AutoTarget::InsertPan(_) => (-1.0, 1.0),
            AutoTarget::ChannelCut(_) => (80.0, 16000.0),
            AutoTarget::InsertFxMix(_, _) => (0.0, 1.0),
        }
    }

    pub fn default_val(&self) -> f32 {
        match self {
            AutoTarget::ChannelVol(_) | AutoTarget::InsertVol(_) | AutoTarget::MasterVol => 0.9,
            AutoTarget::ChannelPan(_) | AutoTarget::InsertPan(_) => 0.0,
            AutoTarget::ChannelCut(_) => 2000.0,
            AutoTarget::InsertFxMix(_, _) => 1.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CurveMode {
    #[default]
    Linear,
    Smooth,
    Hold,
    Exp,
}

impl CurveMode {
    pub const ALL: [CurveMode; 4] = [
        CurveMode::Linear,
        CurveMode::Smooth,
        CurveMode::Hold,
        CurveMode::Exp,
    ];
    pub fn name(&self) -> &'static str {
        match self {
            CurveMode::Linear => "Linear",
            CurveMode::Smooth => "S-Curve",
            CurveMode::Hold => "Hold",
            CurveMode::Exp => "Exp",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AutoClipPoint {
    pub tick: u32,
    pub value: f32,
    #[serde(default)]
    pub tension: f32,
    #[serde(default)]
    pub curve: CurveMode,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MapCurve {
    #[default]
    Linear,
    Log,
    Inverted,
}

impl MapCurve {
    pub const ALL: [MapCurve; 3] = [MapCurve::Linear, MapCurve::Log, MapCurve::Inverted];
    pub fn name(&self) -> &'static str {
        match self {
            MapCurve::Linear => "Linear",
            MapCurve::Log => "Log",
            MapCurve::Inverted => "Inverted",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MidiMapping {
    pub cc: u8,
    pub channel: Option<u8>,
    pub target: AutoTarget,
    pub curve: MapCurve,
    pub min: f32,
    pub max: f32,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PeakController {
    pub name: String,
    pub source_insert: usize,
    pub target: AutoTarget,
    pub amount: f32,
    pub base: f32,
    pub decay_ms: f32,
    pub lfo_hz: f32,
    pub lfo_amt: f32,
    pub enabled: bool,
}

impl Default for PeakController {
    fn default() -> Self {
        Self {
            name: "Peak Ctrl".into(),
            source_insert: 1,
            target: AutoTarget::ChannelVol(4),
            amount: -0.5,
            base: 0.9,
            decay_ms: 80.0,
            lfo_hz: 1.0,
            lfo_amt: 0.0,
            enabled: true,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum ClipKind {
    Pattern {
        pattern: usize,
    },
    Audio {
        sample: usize,
        gain: f32,
        #[serde(default)]
        pitch_semitones: f32,
        #[serde(default)]
        fade_in_ticks: u32,
        #[serde(default)]
        fade_out_ticks: u32,
    },
    Automation {
        target: AutoTarget,
        points: Vec<AutoClipPoint>,
    },
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: u64,
    pub track: usize,
    pub start_tick: u32,
    pub len_ticks: u32,
    #[serde(default)]
    pub offset_ticks: u32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub color: Option<[u8; 3]>,
    pub kind: ClipKind,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MixerRoute {
    pub target: usize,
    pub amount: f32,
    #[serde(default)]
    pub sidechain_only: bool,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct MixerTrack {
    pub name: String,
    pub color: [u8; 3],
    pub volume: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub delay_send: f32,
    pub reverb_send: f32,
    #[serde(default)]
    pub latency_samples: usize,
    #[serde(default)]
    pub routes: Vec<MixerRoute>,
    #[serde(default)]
    pub fx: Vec<EffectSlot>,
    #[serde(default)]
    pub polarity_inv: bool,
    #[serde(default)]
    pub stereo_sep: f32,
}

impl MixerTrack {
    pub fn master() -> Self {
        Self {
            name: "Master".into(),
            color: [220, 150, 70],
            volume: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            delay_send: 0.0,
            reverb_send: 0.0,
            latency_samples: 0,
            routes: Vec::new(),
            fx: Vec::new(),
            polarity_inv: false,
            stereo_sep: 0.0,
        }
    }

    pub fn insert(idx: usize) -> Self {
        Self {
            name: format!("Insert {idx}"),
            color: palette_color(idx),
            volume: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            delay_send: 0.0,
            reverb_send: 0.0,
            latency_samples: 0,
            routes: vec![MixerRoute {
                target: 0,
                amount: 1.0,
                sidechain_only: false,
            }],
            fx: Vec::new(),
            polarity_inv: false,
            stereo_sep: 0.0,
        }
    }

    pub fn effective_latency_samples(&self) -> usize {
        self.latency_samples
            .saturating_add(self.fx.iter().map(EffectSlot::latency_samples).sum())
    }

    pub fn add_fx(&mut self, slot: EffectSlot) -> bool {
        if self.fx.len() >= MAX_FX_SLOTS {
            return false;
        }
        self.fx.push(slot);
        true
    }

    pub fn move_fx(&mut self, from: usize, to: usize) -> bool {
        if from >= self.fx.len() || to >= self.fx.len() || from == to {
            return false;
        }
        let s = self.fx.remove(from);
        self.fx.insert(to, s);
        true
    }
}

pub struct PdcPlan {
    pub order: Vec<usize>,
    pub ch_delay: Vec<usize>,
    pub direct_delay: Vec<usize>,
    pub route_delay: Vec<Vec<(usize, usize)>>,
    pub total_out: Vec<usize>,
}

pub fn palette_color(idx: usize) -> [u8; 3] {
    const PALETTE: [[u8; 3]; 8] = [
        [95, 165, 225],
        [105, 195, 115],
        [225, 160, 80],
        [185, 125, 225],
        [225, 110, 125],
        [90, 195, 195],
        [210, 200, 95],
        [150, 165, 215],
    ];
    PALETTE[idx % PALETTE.len()]
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Song {
    pub save_version: u32,
    pub title: String,
    pub bpm: f32,
    pub swing: f32,
    pub master_vol: f32,
    pub metronome: bool,
    pub loop_song: bool,
    pub delay_time: f32,
    pub delay_mix: f32,
    pub reverb_mix: f32,
    #[serde(default)]
    pub time_sig: TimeSig,
    pub channels: Vec<Channel>,
    pub patterns: Vec<Pattern>,
    pub playlist_tracks: Vec<PlaylistTrack>,
    pub clips: Vec<Clip>,
    pub mixer_tracks: Vec<MixerTrack>,
    pub next_clip_id: u64,
    pub samples: Vec<SampleRef>,
    #[serde(default)]
    pub midi_mappings: Vec<MidiMapping>,
    #[serde(default)]
    pub peak_controllers: Vec<PeakController>,
    #[serde(default)]
    pub count_in_bars: u8,
    #[serde(default)]
    pub auto_quantize_rec: bool,
    #[serde(default)]
    pub punch_in_tick: Option<u32>,
    #[serde(default)]
    pub punch_out_tick: Option<u32>,
}

// Legacy v1 representation used solely for automatic v1 -> v2 loading.
#[derive(Deserialize)]
struct AudioClipV1 {
    bar: usize,
    sample: usize,
    gain: f32,
}

#[derive(Deserialize)]
struct SongV1 {
    save_version: u32,
    title: String,
    bpm: f32,
    swing: f32,
    master_vol: f32,
    metronome: bool,
    loop_song: bool,
    delay_time: f32,
    delay_mix: f32,
    reverb_mix: f32,
    channels: Vec<Channel>,
    patterns: Vec<Pattern>,
    #[serde(default)]
    arrangement: Vec<i32>,
    #[serde(default)]
    audio_clips: Vec<AudioClipV1>,
    samples: Vec<SampleRef>,
}

fn default_mixer_tracks() -> Vec<MixerTrack> {
    let mut out = Vec::with_capacity(DEFAULT_MIXER_INSERTS + 1);
    out.push(MixerTrack::master());
    for i in 1..=DEFAULT_MIXER_INSERTS {
        out.push(MixerTrack::insert(i));
    }
    out
}

impl Song {
    /// Step-grid view derived from notes (single source of truth = notes).
    pub fn step_active(&self, pat: usize, ch: usize, step: usize) -> bool {
        let t0 = step as u32 * TICKS_PER_STEP;
        self.patterns
            .get(pat)
            .and_then(|p| p.notes.get(ch))
            .map(|ns| {
                ns.iter()
                    .any(|n| n.tick >= t0 && n.tick < t0 + TICKS_PER_STEP)
            })
            .unwrap_or(false)
    }

    /// Toggle a step: remove notes starting in the step window, else add base note.
    pub fn toggle_step(&mut self, pat: usize, ch: usize, step: usize) {
        let t0 = step as u32 * TICKS_PER_STEP;
        let max_ticks = self
            .patterns
            .get(pat)
            .map(|p| p.len_ticks())
            .unwrap_or(TICKS_PATTERN);
        if t0 >= max_ticks {
            return;
        }
        let base = self.channels.get(ch).map(|c| c.midi_base).unwrap_or(60);
        if let Some(notes) = self.patterns.get_mut(pat).and_then(|p| p.notes.get_mut(ch)) {
            if notes
                .iter()
                .any(|n| n.tick >= t0 && n.tick < t0 + TICKS_PER_STEP)
            {
                notes.retain(|n| n.tick < t0 || n.tick >= t0 + TICKS_PER_STEP);
            } else {
                notes.push(Note::new_in(t0, TICKS_PER_STEP, base, 0.9, max_ticks));
                notes.sort_by_key(|n| n.tick);
            }
        }
    }

    /// Set a step explicitly (used for drag-painting or right-drag erasing).
    pub fn set_step(&mut self, pat: usize, ch: usize, step: usize, active: bool) -> bool {
        let t0 = step as u32 * TICKS_PER_STEP;
        let max_ticks = self
            .patterns
            .get(pat)
            .map(|p| p.len_ticks())
            .unwrap_or(TICKS_PATTERN);
        if t0 >= max_ticks {
            return false;
        }
        let base = self.channels.get(ch).map(|c| c.midi_base).unwrap_or(60);
        let Some(notes) = self.patterns.get_mut(pat).and_then(|p| p.notes.get_mut(ch)) else {
            return false;
        };
        let has = notes
            .iter()
            .any(|n| n.tick >= t0 && n.tick < t0 + TICKS_PER_STEP);
        if active && !has {
            notes.push(Note::new_in(t0, TICKS_PER_STEP, base, 0.9, max_ticks));
            notes.sort_by_key(|n| n.tick);
            true
        } else if !active && has {
            notes.retain(|n| n.tick < t0 || n.tick >= t0 + TICKS_PER_STEP);
            true
        } else {
            false
        }
    }

    pub fn alloc_clip_id(&mut self) -> u64 {
        let id = self.next_clip_id.max(1);
        self.next_clip_id = id + 1;
        id
    }

    pub fn add_channel(&mut self, mut ch: Channel) -> usize {
        let idx = self.channels.len();
        if ch.mixer_insert >= self.mixer_tracks.len() {
            ch.mixer_insert = 0;
        }
        self.channels.push(ch);
        for p in &mut self.patterns {
            p.notes.push(Vec::new());
            p.auto_vol.push(Vec::new());
            p.auto_cut.push(Vec::new());
        }
        idx
    }

    pub fn duplicate_channel(&mut self, idx: usize) -> Option<usize> {
        let orig = self.channels.get(idx)?.clone();
        let mut copy = orig;
        copy.name = format!("{} copy", copy.name);
        if let ChannelKind::ClapPlugin(cfg) = &mut copy.kind {
            cfg.rt_id = next_clap_rt_id();
        }
        let new_idx = self.channels.len();
        self.channels.push(copy);
        for p in &mut self.patterns {
            let notes = p.notes.get(idx).cloned().unwrap_or_default();
            let av = p.auto_vol.get(idx).cloned().unwrap_or_default();
            let ac = p.auto_cut.get(idx).cloned().unwrap_or_default();
            p.notes.push(notes);
            p.auto_vol.push(av);
            p.auto_cut.push(ac);
        }
        Some(new_idx)
    }

    pub fn delete_channel(&mut self, idx: usize) -> bool {
        if self.channels.len() <= 1 || idx >= self.channels.len() {
            return false;
        }
        self.channels.remove(idx);
        for p in &mut self.patterns {
            if idx < p.notes.len() {
                p.notes.remove(idx);
            }
            if idx < p.auto_vol.len() {
                p.auto_vol.remove(idx);
            }
            if idx < p.auto_cut.len() {
                p.auto_cut.remove(idx);
            }
        }
        for t in &mut self.playlist_tracks {
            if let Some(lc) = t.linked_channel {
                t.linked_channel = match lc.cmp(&idx) {
                    std::cmp::Ordering::Equal => None,
                    std::cmp::Ordering::Greater => Some(lc - 1),
                    std::cmp::Ordering::Less => Some(lc),
                };
            }
        }
        true
    }

    pub fn move_channel(&mut self, from: usize, to: usize) -> bool {
        let n = self.channels.len();
        if from >= n || to >= n || from == to {
            return false;
        }
        let ch = self.channels.remove(from);
        self.channels.insert(to, ch);
        for p in &mut self.patterns {
            if from < p.notes.len() && to < p.notes.len() {
                let v = p.notes.remove(from);
                p.notes.insert(to, v);
            }
            if from < p.auto_vol.len() && to < p.auto_vol.len() {
                let v = p.auto_vol.remove(from);
                p.auto_vol.insert(to, v);
            }
            if from < p.auto_cut.len() && to < p.auto_cut.len() {
                let v = p.auto_cut.remove(from);
                p.auto_cut.insert(to, v);
            }
        }
        for t in &mut self.playlist_tracks {
            if let Some(lc) = t.linked_channel {
                t.linked_channel = Some(if lc == from {
                    to
                } else if from < to && lc > from && lc <= to {
                    lc - 1
                } else if to < from && lc >= to && lc < from {
                    lc + 1
                } else {
                    lc
                });
            }
        }
        true
    }

    pub fn add_pattern(&mut self, name: Option<String>) -> usize {
        let idx = self.patterns.len();
        let label = name.unwrap_or_else(|| format!("Pattern {}", idx + 1));
        self.patterns.push(Pattern::empty_for_channels(
            label,
            palette_color(idx),
            self.channels.len(),
            self.time_sig,
        ));
        idx
    }

    pub fn clone_pattern(&mut self, idx: usize) -> Option<usize> {
        let mut p = self.patterns.get(idx)?.clone();
        let new_idx = self.patterns.len();
        p.name = format!("{} copy", p.name);
        p.color = palette_color(new_idx);
        self.patterns.push(p);
        Some(new_idx)
    }

    pub fn delete_pattern(&mut self, idx: usize) -> bool {
        if self.patterns.len() <= 1 || idx >= self.patterns.len() {
            return false;
        }
        self.patterns.remove(idx);
        self.clips.retain(|c| match c.kind {
            ClipKind::Pattern { pattern } => pattern != idx,
            _ => true,
        });
        for c in &mut self.clips {
            if let ClipKind::Pattern { pattern } = &mut c.kind
                && *pattern > idx
            {
                *pattern -= 1;
            }
        }
        true
    }

    pub fn make_clip_unique(&mut self, clip_id: u64) -> Option<usize> {
        let ci = self.clips.iter().position(|c| c.id == clip_id)?;
        let src_pat = match self.clips[ci].kind {
            ClipKind::Pattern { pattern } => pattern,
            _ => return None,
        };
        let new_pat = self.clone_pattern(src_pat)?;
        self.clips[ci].kind = ClipKind::Pattern { pattern: new_pat };
        Some(new_pat)
    }

    pub fn add_playlist_track(&mut self) -> usize {
        let idx = self.playlist_tracks.len();
        self.playlist_tracks.push(PlaylistTrack::new(idx));
        idx
    }

    pub fn add_pattern_clip(&mut self, track: usize, start_tick: u32, pattern: usize) -> u64 {
        while self.playlist_tracks.len() <= track {
            self.add_playlist_track();
        }
        let len_ticks = self
            .patterns
            .get(pattern)
            .map(|p| p.len_ticks())
            .unwrap_or(TICKS_PATTERN);
        let id = self.alloc_clip_id();
        self.clips.push(Clip {
            id,
            track,
            start_tick,
            len_ticks,
            offset_ticks: 0,
            mute: false,
            color: None,
            kind: ClipKind::Pattern { pattern },
        });
        id
    }

    pub fn add_audio_clip(
        &mut self,
        track: usize,
        start_tick: u32,
        sample: usize,
        gain: f32,
    ) -> u64 {
        while self.playlist_tracks.len() <= track {
            self.add_playlist_track();
        }
        let ticks_from_sample = self
            .samples
            .get(sample)
            .filter(|s| s.sr > 0.0 && !s.data.is_empty())
            .map(|s| {
                let secs = s.data.len() as f32 / s.sr;
                let tps = (self.bpm.max(30.0) / 60.0) * 16.0;
                ((secs * tps).ceil() as u32).max(TICKS_PER_STEP * 4)
            })
            .unwrap_or(self.time_sig.ticks_per_bar());
        let id = self.alloc_clip_id();
        self.clips.push(Clip {
            id,
            track,
            start_tick,
            len_ticks: ticks_from_sample,
            offset_ticks: 0,
            mute: false,
            color: None,
            kind: ClipKind::Audio {
                sample,
                gain,
                pitch_semitones: 0.0,
                fade_in_ticks: 0,
                fade_out_ticks: 0,
            },
        });
        id
    }

    pub fn add_auto_clip(
        &mut self,
        track: usize,
        start_tick: u32,
        len_ticks: u32,
        target: AutoTarget,
    ) -> u64 {
        while self.playlist_tracks.len() <= track {
            self.add_playlist_track();
        }
        let (lo, hi) = target.range();
        let init_val = self
            .target_value(target)
            .unwrap_or(target.default_val())
            .clamp(lo, hi);
        let len = len_ticks.max(TICKS_PER_STEP * 4);
        let id = self.alloc_clip_id();
        self.clips.push(Clip {
            id,
            track,
            start_tick,
            len_ticks: len,
            offset_ticks: 0,
            mute: false,
            color: Some([210, 140, 225]),
            kind: ClipKind::Automation {
                target,
                points: vec![
                    AutoClipPoint {
                        tick: 0,
                        value: init_val,
                        tension: 0.0,
                        curve: CurveMode::Linear,
                    },
                    AutoClipPoint {
                        tick: len,
                        value: init_val,
                        tension: 0.0,
                        curve: CurveMode::Linear,
                    },
                ],
            },
        });
        id
    }

    pub fn target_value(&self, target: AutoTarget) -> Option<f32> {
        match target {
            AutoTarget::ChannelVol(ci) => self.channels.get(ci).map(|c| c.volume),
            AutoTarget::ChannelPan(ci) => self.channels.get(ci).map(|c| c.pan),
            AutoTarget::ChannelCut(ci) => self.channels.get(ci).map(|c| c.cutoff),
            AutoTarget::InsertVol(ins) => self.mixer_tracks.get(ins).map(|m| m.volume),
            AutoTarget::InsertPan(ins) => self.mixer_tracks.get(ins).map(|m| m.pan),
            AutoTarget::InsertFxMix(ins, slot) => self
                .mixer_tracks
                .get(ins)
                .and_then(|m| m.fx.get(slot))
                .map(|f| f.mix),
            AutoTarget::MasterVol => Some(self.master_vol),
        }
    }

    pub fn apply_target_value(&mut self, target: AutoTarget, value: f32) -> bool {
        let (lo, hi) = target.range();
        let v = value.clamp(lo, hi);
        match target {
            AutoTarget::ChannelVol(ci) => {
                if let Some(ch) = self.channels.get_mut(ci) {
                    ch.volume = v;
                    return true;
                }
            }
            AutoTarget::ChannelPan(ci) => {
                if let Some(ch) = self.channels.get_mut(ci) {
                    ch.pan = v;
                    return true;
                }
            }
            AutoTarget::ChannelCut(ci) => {
                if let Some(ch) = self.channels.get_mut(ci) {
                    ch.cutoff = v;
                    return true;
                }
            }
            AutoTarget::InsertVol(ins) => {
                if let Some(tr) = self.mixer_tracks.get_mut(ins) {
                    tr.volume = v;
                    return true;
                }
            }
            AutoTarget::InsertPan(ins) => {
                if let Some(tr) = self.mixer_tracks.get_mut(ins) {
                    tr.pan = v;
                    return true;
                }
            }
            AutoTarget::InsertFxMix(ins, slot) => {
                if let Some(fx) = self
                    .mixer_tracks
                    .get_mut(ins)
                    .and_then(|m| m.fx.get_mut(slot))
                {
                    fx.mix = v;
                    return true;
                }
            }
            AutoTarget::MasterVol => {
                self.master_vol = v;
                return true;
            }
        }
        false
    }

    pub fn remove_clip(&mut self, id: u64) -> bool {
        let before = self.clips.len();
        self.clips.retain(|c| c.id != id);
        self.clips.len() != before
    }

    pub fn playlist_len_ticks(&self) -> u32 {
        self.clips
            .iter()
            .filter(|c| !c.mute)
            .map(|c| c.start_tick.saturating_add(c.len_ticks))
            .max()
            .unwrap_or(0)
    }

    pub fn add_mixer_insert(&mut self) -> usize {
        let idx = self.mixer_tracks.len();
        self.mixer_tracks.push(MixerTrack::insert(idx));
        idx
    }

    pub fn mixer_would_cycle(&self, from: usize, to: usize) -> bool {
        if from == to {
            return true;
        }
        // Does `to` already reach `from`?
        let mut stack = vec![to];
        let mut visited = vec![false; self.mixer_tracks.len()];
        while let Some(u) = stack.pop() {
            if u == from {
                return true;
            }
            if u >= visited.len() || visited[u] {
                continue;
            }
            visited[u] = true;
            for r in &self.mixer_tracks[u].routes {
                stack.push(r.target);
            }
        }
        false
    }

    pub fn add_mixer_route(
        &mut self,
        from: usize,
        to: usize,
        amount: f32,
        sidechain_only: bool,
    ) -> Result<(), String> {
        let n = self.mixer_tracks.len();
        if from == 0 {
            return Err("Master track cannot route out".into());
        }
        if from >= n || to >= n {
            return Err("mixer track out of range".into());
        }
        if self.mixer_would_cycle(from, to) {
            return Err(format!("route Insert {from} -> {to} would create a cycle"));
        }
        let routes = &mut self.mixer_tracks[from].routes;
        if let Some(existing) = routes.iter_mut().find(|r| r.target == to) {
            existing.amount = amount.clamp(0.0, 1.5);
            existing.sidechain_only = sidechain_only;
        } else {
            routes.push(MixerRoute {
                target: to,
                amount: amount.clamp(0.0, 1.5),
                sidechain_only,
            });
        }
        Ok(())
    }

    pub fn remove_mixer_route(&mut self, from: usize, to: usize) {
        if let Some(t) = self.mixer_tracks.get_mut(from) {
            t.routes.retain(|r| r.target != to);
        }
    }

    /// Topological order of mixer tracks from upstream sources down to Master (0).
    pub fn mixer_topo_order(&self) -> Vec<usize> {
        let n = self.mixer_tracks.len();
        let mut indeg = vec![0usize; n];
        for t in &self.mixer_tracks {
            for r in &t.routes {
                if r.target < n {
                    indeg[r.target] += 1;
                }
            }
        }
        let mut q: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        let mut order = Vec::with_capacity(n);
        let mut head = 0;
        while head < q.len() {
            let u = q[head];
            head += 1;
            order.push(u);
            for r in &self.mixer_tracks[u].routes {
                if r.target < n {
                    indeg[r.target] -= 1;
                    if indeg[r.target] == 0 {
                        q.push(r.target);
                    }
                }
            }
        }
        // Safety fallback if any external edit formed a cycle: append remaining.
        if order.len() < n {
            for i in 0..n {
                if !order.contains(&i) {
                    order.push(i);
                }
            }
        }
        order
    }

    /// Automatic Plugin Delay Compensation across channels and parallel routed mixer paths.
    pub fn compute_pdc(&self) -> PdcPlan {
        let n = self.mixer_tracks.len();
        let order = self.mixer_topo_order();
        let mut incoming_non_sc: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
        for (u, tr) in self.mixer_tracks.iter().enumerate().take(n) {
            for (ri, r) in tr.routes.iter().enumerate() {
                if !r.sidechain_only && r.target < n {
                    incoming_non_sc[r.target].push((u, ri));
                }
            }
        }
        let mut ch_lat = vec![0usize; n];
        for ch in &self.channels {
            let lat = match &ch.kind {
                ChannelKind::ClapPlugin(c) => c.reported_latency,
                _ => 0,
            };
            if ch.mixer_insert < n {
                ch_lat[ch.mixer_insert] = ch_lat[ch.mixer_insert].max(lat);
            }
        }
        let ch_delay = self
            .channels
            .iter()
            .map(|ch| {
                let lat = match &ch.kind {
                    ChannelKind::ClapPlugin(c) => c.reported_latency,
                    _ => 0,
                };
                if ch.mixer_insert < n {
                    ch_lat[ch.mixer_insert].saturating_sub(lat)
                } else {
                    0
                }
            })
            .collect();

        let mut direct_delay = vec![0usize; n];
        let mut total_out = vec![0usize; n];
        let mut route_delay: Vec<Vec<(usize, usize)>> = self
            .mixer_tracks
            .iter()
            .map(|t| t.routes.iter().map(|r| (r.target, 0)).collect())
            .collect();

        for &v in &order {
            let max_in = incoming_non_sc[v]
                .iter()
                .map(|&(u, _)| total_out[u])
                .max()
                .unwrap_or(0);
            let aligned_in = max_in.max(ch_lat[v]);
            direct_delay[v] = aligned_in.saturating_sub(ch_lat[v]);
            for &(u, ri) in &incoming_non_sc[v] {
                route_delay[u][ri].1 = aligned_in.saturating_sub(total_out[u]);
            }
            total_out[v] =
                aligned_in.saturating_add(self.mixer_tracks[v].effective_latency_samples());
        }

        PdcPlan {
            order,
            ch_delay,
            direct_delay,
            route_delay,
            total_out,
        }
    }

    pub fn save(&self, path: &str) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }

    pub fn from_json_str(text: &str) -> Result<Song, String> {
        let val: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let ver = val
            .get("save_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let mut song = match ver {
            1 => {
                let v1: SongV1 = serde_json::from_value(val).map_err(|e| e.to_string())?;
                migrate_v1_to_v2(v1)
            }
            SAVE_VERSION => serde_json::from_value::<Song>(val).map_err(|e| e.to_string())?,
            other => return Err(format!("unsupported project version {other}")),
        };
        song.normalize();
        Ok(song)
    }

    pub fn load(path: &str) -> Result<Song, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Self::from_json_str(&text)
    }

    fn normalize(&mut self) {
        if self.mixer_tracks.is_empty() {
            self.mixer_tracks = default_mixer_tracks();
        }
        let nmix = self.mixer_tracks.len();
        for ch in &mut self.channels {
            if ch.mixer_insert >= nmix {
                ch.mixer_insert = 0;
            }
        }
        if self.patterns.is_empty() {
            self.patterns.push(Pattern::empty_for_channels(
                "Pattern 1".into(),
                palette_color(0),
                self.channels.len(),
                self.time_sig,
            ));
        }
        let nch = self.channels.len();
        for (i, p) in self.patterns.iter_mut().enumerate() {
            if p.name.is_empty() || p.name == "Pattern" {
                p.name = format!("Pattern {}", i + 1);
            }
            p.bars = p.bars.max(1);
            p.notes.resize_with(nch, Vec::new);
            p.auto_vol.resize_with(nch, Vec::new);
            p.auto_cut.resize_with(nch, Vec::new);
        }
        if self.playlist_tracks.is_empty() {
            for i in 0..4 {
                self.playlist_tracks.push(PlaylistTrack::new(i));
            }
        }
        let max_id = self.clips.iter().map(|c| c.id).max().unwrap_or(0);
        if self.next_clip_id <= max_id {
            self.next_clip_id = max_id + 1;
        }
    }
}

fn migrate_v1_to_v2(v1: SongV1) -> Song {
    let mut tracks = vec![
        PlaylistTrack {
            name: "Patterns".into(),
            color: palette_color(0),
            mute: false,
            solo: false,
            linked_channel: None,
            linked_insert: None,
        },
        PlaylistTrack {
            name: "Audio".into(),
            color: palette_color(1),
            mute: false,
            solo: false,
            linked_channel: None,
            linked_insert: None,
        },
        PlaylistTrack::new(2),
        PlaylistTrack::new(3),
    ];
    let mut clips = Vec::new();
    let mut next_id = 1u64;
    for (bar, &pat) in v1.arrangement.iter().enumerate() {
        if pat >= 0 {
            let p_idx = pat as usize;
            let len_ticks = v1
                .patterns
                .get(p_idx)
                .map(|p| p.len_ticks())
                .unwrap_or(TICKS_PATTERN);
            clips.push(Clip {
                id: next_id,
                track: 0,
                start_tick: bar as u32 * TICKS_PATTERN,
                len_ticks,
                offset_ticks: 0,
                mute: false,
                color: None,
                kind: ClipKind::Pattern { pattern: p_idx },
            });
            next_id += 1;
        }
    }
    for ac in v1.audio_clips {
        clips.push(Clip {
            id: next_id,
            track: 1,
            start_tick: ac.bar as u32 * TICKS_PATTERN,
            len_ticks: TICKS_PATTERN,
            offset_ticks: 0,
            mute: false,
            color: None,
            kind: ClipKind::Audio {
                sample: ac.sample,
                gain: ac.gain,
                pitch_semitones: 0.0,
                fade_in_ticks: 0,
                fade_out_ticks: 0,
            },
        });
        next_id += 1;
    }
    let _ = &mut tracks;
    let mut channels = v1.channels;
    for (i, ch) in channels.iter_mut().enumerate() {
        ch.mixer_insert = (i + 1).min(DEFAULT_MIXER_INSERTS);
    }
    let mut patterns = v1.patterns;
    for (i, p) in patterns.iter_mut().enumerate() {
        p.name = format!("Pattern {}", i + 1);
        p.color = palette_color(i);
        p.bars = 1;
        p.time_sig = TimeSig::default();
    }
    let _ = v1.save_version;
    Song {
        save_version: SAVE_VERSION,
        title: v1.title,
        bpm: v1.bpm,
        swing: v1.swing,
        master_vol: v1.master_vol,
        metronome: v1.metronome,
        loop_song: v1.loop_song,
        delay_time: v1.delay_time,
        delay_mix: v1.delay_mix,
        reverb_mix: v1.reverb_mix,
        time_sig: TimeSig::default(),
        channels,
        patterns,
        playlist_tracks: tracks,
        clips,
        mixer_tracks: default_mixer_tracks(),
        next_clip_id: next_id,
        samples: v1.samples,
        midi_mappings: Vec::new(),
        peak_controllers: Vec::new(),
        count_in_bars: 0,
        auto_quantize_rec: false,
        punch_in_tick: None,
        punch_out_tick: None,
    }
}

pub fn map_cc_value(val: u8, m: &MidiMapping) -> f32 {
    let u = (val as f32 / 127.0).clamp(0.0, 1.0);
    let (lo, hi) = m.target.range();
    let min = m.min.clamp(lo, hi);
    let max = m.max.clamp(lo, hi);
    match m.curve {
        MapCurve::Linear => min + (max - min) * u,
        MapCurve::Inverted => max - (max - min) * u,
        MapCurve::Log => {
            if min > 0.0 && max > min {
                min * (max / min).powf(u)
            } else {
                min + (max - min) * u * u
            }
        }
    }
}

/// Linear automation eval. Returns None when there are no points.
pub fn eval_auto(points: &[AutoPoint], tick: u32) -> Option<f32> {
    if points.is_empty() {
        return None;
    }
    let mut pts = points.to_vec();
    pts.sort_by_key(|p| p.tick);
    if tick <= pts[0].tick {
        return Some(pts[0].value);
    }
    for w in pts.windows(2) {
        if tick >= w[0].tick && tick <= w[1].tick {
            let span = (w[1].tick - w[0].tick).max(1) as f32;
            let t = (tick - w[0].tick) as f32 / span;
            return Some(w[0].value + (w[1].value - w[0].value) * t);
        }
    }
    pts.last().map(|p| p.value)
}

/// Evaluate an AutomationClip's curved points at local clip tick `t_local`.
pub fn eval_auto_clip(points: &[AutoClipPoint], t_local: f32) -> Option<f32> {
    if points.is_empty() {
        return None;
    }
    if t_local <= points[0].tick as f32 {
        return Some(points[0].value);
    }
    for w in points.windows(2) {
        let t0 = w[0].tick as f32;
        let t1 = w[1].tick as f32;
        if t_local >= t0 && t_local < t1 {
            let span = (t1 - t0).max(1e-4);
            let u = ((t_local - t0) / span).clamp(0.0, 1.0);
            let ten = w[0].tension.clamp(-1.0, 1.0);
            let shaped = match w[0].curve {
                CurveMode::Hold => return Some(w[0].value),
                CurveMode::Linear => {
                    if ten.abs() < 1e-3 {
                        u
                    } else if ten > 0.0 {
                        u.powf(1.0 + ten * 3.0)
                    } else {
                        1.0 - (1.0 - u).powf(1.0 + (-ten) * 3.0)
                    }
                }
                CurveMode::Smooth => {
                    let s = u * u * (3.0 - 2.0 * u);
                    if ten.abs() < 1e-3 {
                        s
                    } else if ten > 0.0 {
                        s.powf(1.0 + ten * 2.0)
                    } else {
                        1.0 - (1.0 - s).powf(1.0 + (-ten) * 2.0)
                    }
                }
                CurveMode::Exp => {
                    let k = 2.0 + ten.abs() * 4.0;
                    if ten >= 0.0 {
                        ((k * u).exp() - 1.0) / (k.exp() - 1.0)
                    } else {
                        1.0 - ((k * (1.0 - u)).exp() - 1.0) / (k.exp() - 1.0)
                    }
                }
            };
            return Some(w[0].value + (w[1].value - w[0].value) * shaped);
        }
    }
    points.last().map(|p| p.value)
}

// ---------------------------------------------------------------------------
// Piano Roll scales, chords, snap grids, and note tools
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScaleKind {
    Chromatic,
    Major,
    Minor,
    HarmonicMinor,
    MelodicMinor,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
    PentMaj,
    PentMin,
    Blues,
}

impl ScaleKind {
    pub const ALL: [ScaleKind; 12] = [
        ScaleKind::Chromatic,
        ScaleKind::Major,
        ScaleKind::Minor,
        ScaleKind::HarmonicMinor,
        ScaleKind::MelodicMinor,
        ScaleKind::Dorian,
        ScaleKind::Phrygian,
        ScaleKind::Lydian,
        ScaleKind::Mixolydian,
        ScaleKind::PentMaj,
        ScaleKind::PentMin,
        ScaleKind::Blues,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            ScaleKind::Chromatic => "Chromatic",
            ScaleKind::Major => "Major",
            ScaleKind::Minor => "Natural Minor",
            ScaleKind::HarmonicMinor => "Harmonic Minor",
            ScaleKind::MelodicMinor => "Melodic Minor",
            ScaleKind::Dorian => "Dorian",
            ScaleKind::Phrygian => "Phrygian",
            ScaleKind::Lydian => "Lydian",
            ScaleKind::Mixolydian => "Mixolydian",
            ScaleKind::PentMaj => "Pent Maj",
            ScaleKind::PentMin => "Pent Min",
            ScaleKind::Blues => "Blues",
        }
    }

    pub fn contains(&self, pc: u8, root: u8) -> bool {
        let rel = (12 + pc as i32 - (root % 12) as i32) % 12;
        match self {
            ScaleKind::Chromatic => true,
            ScaleKind::Major => matches!(rel, 0 | 2 | 4 | 5 | 7 | 9 | 11),
            ScaleKind::Minor => matches!(rel, 0 | 2 | 3 | 5 | 7 | 8 | 10),
            ScaleKind::HarmonicMinor => matches!(rel, 0 | 2 | 3 | 5 | 7 | 8 | 11),
            ScaleKind::MelodicMinor => matches!(rel, 0 | 2 | 3 | 5 | 7 | 9 | 11),
            ScaleKind::Dorian => matches!(rel, 0 | 2 | 3 | 5 | 7 | 9 | 10),
            ScaleKind::Phrygian => matches!(rel, 0 | 1 | 3 | 5 | 7 | 8 | 10),
            ScaleKind::Lydian => matches!(rel, 0 | 2 | 4 | 6 | 7 | 9 | 11),
            ScaleKind::Mixolydian => matches!(rel, 0 | 2 | 4 | 5 | 7 | 9 | 10),
            ScaleKind::PentMaj => matches!(rel, 0 | 2 | 4 | 7 | 9),
            ScaleKind::PentMin => matches!(rel, 0 | 3 | 5 | 7 | 10),
            ScaleKind::Blues => matches!(rel, 0 | 3 | 5 | 6 | 7 | 10),
        }
    }

    pub fn snap_midi(&self, midi: u8, root: u8) -> u8 {
        if *self == ScaleKind::Chromatic {
            return midi;
        }
        let mut m = midi;
        for _ in 0..12 {
            if self.contains(m % 12, root) {
                return m;
            }
            m = m.saturating_sub(1).max(21);
        }
        midi
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChordKind {
    Off,
    Maj,
    Min,
    Dim,
    Aug,
    Sus2,
    Sus4,
    Maj7,
    Min7,
    Dom7,
    Maj9,
}

impl ChordKind {
    pub const ALL: [ChordKind; 11] = [
        ChordKind::Off,
        ChordKind::Maj,
        ChordKind::Min,
        ChordKind::Dim,
        ChordKind::Aug,
        ChordKind::Sus2,
        ChordKind::Sus4,
        ChordKind::Maj7,
        ChordKind::Min7,
        ChordKind::Dom7,
        ChordKind::Maj9,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            ChordKind::Off => "Off",
            ChordKind::Maj => "Maj",
            ChordKind::Min => "Min",
            ChordKind::Dim => "Dim",
            ChordKind::Aug => "Aug",
            ChordKind::Sus2 => "Sus2",
            ChordKind::Sus4 => "Sus4",
            ChordKind::Maj7 => "Maj7",
            ChordKind::Min7 => "Min7",
            ChordKind::Dom7 => "Dom7",
            ChordKind::Maj9 => "Maj9",
        }
    }

    pub fn intervals(&self) -> &'static [u8] {
        match self {
            ChordKind::Off => &[0],
            ChordKind::Maj => &[0, 4, 7],
            ChordKind::Min => &[0, 3, 7],
            ChordKind::Dim => &[0, 3, 6],
            ChordKind::Aug => &[0, 4, 8],
            ChordKind::Sus2 => &[0, 2, 7],
            ChordKind::Sus4 => &[0, 5, 7],
            ChordKind::Maj7 => &[0, 4, 7, 11],
            ChordKind::Min7 => &[0, 3, 7, 10],
            ChordKind::Dom7 => &[0, 4, 7, 10],
            ChordKind::Maj9 => &[0, 4, 7, 11, 14],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SnapGrid {
    Line,
    Cell,
    S1_64,
    S1_32,
    S1_16,
    S1_8,
    S1_4,
    TripletBeat,
    TripletStep,
    None,
}

impl SnapGrid {
    pub const ALL: [SnapGrid; 10] = [
        SnapGrid::Line,
        SnapGrid::Cell,
        SnapGrid::S1_64,
        SnapGrid::S1_32,
        SnapGrid::S1_16,
        SnapGrid::S1_8,
        SnapGrid::S1_4,
        SnapGrid::TripletBeat,
        SnapGrid::TripletStep,
        SnapGrid::None,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            SnapGrid::Line => "Line",
            SnapGrid::Cell => "Cell",
            SnapGrid::S1_64 => "1/64 (1/4 step)",
            SnapGrid::S1_32 => "1/32 (1/2 step)",
            SnapGrid::S1_16 => "1/16 (Step)",
            SnapGrid::S1_8 => "1/8 (1/2 beat)",
            SnapGrid::S1_4 => "1/4 (Beat)",
            SnapGrid::TripletBeat => "1/3 Beat",
            SnapGrid::TripletStep => "1/3 Step",
            SnapGrid::None => "(none)",
        }
    }

    pub fn step_ticks(&self, cell_w: f32) -> f32 {
        match self {
            SnapGrid::Line => 4.0,
            SnapGrid::Cell => {
                if cell_w >= 26.0 {
                    1.0
                } else if cell_w >= 14.0 {
                    2.0
                } else {
                    4.0
                }
            }
            SnapGrid::S1_64 | SnapGrid::None => 1.0,
            SnapGrid::S1_32 => 2.0,
            SnapGrid::S1_16 => 4.0,
            SnapGrid::S1_8 => 8.0,
            SnapGrid::S1_4 => 16.0,
            SnapGrid::TripletBeat => 16.0 / 3.0,
            SnapGrid::TripletStep => 4.0 / 3.0,
        }
    }

    pub fn snap_floor(&self, tick_f: f32, cell_w: f32, max_ticks: u32) -> u32 {
        let cap = max_ticks.saturating_sub(1);
        if *self == SnapGrid::None {
            return (tick_f.floor() as i32).clamp(0, cap as i32) as u32;
        }
        let step = self.step_ticks(cell_w).max(1.0);
        let q = ((tick_f / step).floor() * step).round() as i32;
        q.clamp(0, cap as i32) as u32
    }

    pub fn snap_nearest(&self, tick_f: f32, cell_w: f32, max_ticks: u32) -> u32 {
        let cap = max_ticks.saturating_sub(1);
        if *self == SnapGrid::None {
            return (tick_f.round() as i32).clamp(0, cap as i32) as u32;
        }
        let step = self.step_ticks(cell_w).max(1.0);
        let q = ((tick_f / step).round() * step).round() as i32;
        q.clamp(0, cap as i32) as u32
    }

    pub fn default_len(&self, cell_w: f32) -> u32 {
        match self {
            SnapGrid::None => TICKS_PER_STEP,
            _ => (self.step_ticks(cell_w).round() as u32).max(1),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ArpDir {
    Up,
    Down,
    UpDown,
}

#[derive(Clone, Copy)]
pub struct QuantizeCfg {
    pub grid: f32,
    pub strength: f32,
    pub swing: f32,
    pub max_ticks: u32,
}

#[derive(Clone, Copy)]
pub struct StrumCfg {
    pub amount: u32,
    pub vel_slope: f32,
    pub preserve_end: bool,
    pub max_ticks: u32,
}

#[derive(Clone, Copy)]
pub struct ArpCfg {
    pub step_ticks: u32,
    pub gate: f32,
    pub octaves: u8,
    pub dir: ArpDir,
    pub max_ticks: u32,
}

#[derive(Clone, Copy)]
pub struct RandCfg {
    pub seed: u64,
    pub pitch_range: i8,
    pub vel_amt: f32,
    pub pan_amt: f32,
}

fn target_mask(len: usize, sel: &[usize]) -> Vec<bool> {
    if sel.is_empty() {
        vec![true; len]
    } else {
        let mut mask = vec![false; len];
        for &idx in sel {
            if idx < len {
                mask[idx] = true;
            }
        }
        mask
    }
}

pub fn tool_quantize_opts(notes: &mut [Note], sel: &[usize], cfg: &QuantizeCfg) {
    let grid = cfg.grid.max(1.0);
    let strength = cfg.strength.clamp(0.0, 1.0);
    let swing = cfg.swing.clamp(0.0, 0.75);
    let cap = cfg.max_ticks.saturating_sub(1);
    let mask = target_mask(notes.len(), sel);
    for (idx, n) in notes.iter_mut().enumerate() {
        if !mask[idx] {
            continue;
        }
        let step_idx = (n.tick as f32 / grid).round();
        let mut target = step_idx * grid;
        if (step_idx as i32).rem_euclid(2) == 1 {
            target += swing * grid * 0.5;
        }
        let blended = n.tick as f32 + (target - n.tick as f32) * strength;
        n.tick = (blended.round() as i32).clamp(0, cap as i32) as u32;
    }
}

pub fn tool_strum_opts(notes: &mut [Note], sel: &[usize], cfg: &StrumCfg) {
    let mask = target_mask(notes.len(), sel);
    let mut order: Vec<usize> = (0..notes.len()).filter(|&i| mask[i]).collect();
    order.sort_by_key(|&i| (notes[i].tick, notes[i].midi));
    let cap = cfg.max_ticks.saturating_sub(1);
    let mut pos = 0usize;
    while pos < order.len() {
        let t0 = notes[order[pos]].tick;
        let mut end = pos;
        while end < order.len() && notes[order[end]].tick.saturating_sub(t0) <= 2 {
            end += 1;
        }
        let cluster = &order[pos..end];
        let denom = (cluster.len().saturating_sub(1)).max(1) as f32;
        for (k, &ni) in cluster.iter().enumerate() {
            let n = &mut notes[ni];
            let old_end = n.tick.saturating_add(n.len);
            let new_t = (t0 + k as u32 * cfg.amount).min(cap);
            n.tick = new_t;
            if cfg.preserve_end {
                n.len = old_end.saturating_sub(new_t).max(1);
            }
            let frac = k as f32 / denom;
            n.vel = (n.vel + (frac - 0.5) * cfg.vel_slope * 0.5).clamp(0.05, 1.0);
        }
        pos = end.max(pos + 1);
    }
}

pub fn tool_flam(notes: &mut Vec<Note>, sel: &[usize], lead_ticks: u32, vel_scale: f32) {
    let lead = lead_ticks.max(1);
    let scale = vel_scale.clamp(0.1, 1.0);
    let mask = target_mask(notes.len(), sel);
    let mut extra = Vec::new();
    for (idx, n) in notes.iter().enumerate() {
        if !mask[idx] {
            continue;
        }
        let mut grace = *n;
        grace.tick = n.tick.saturating_sub(lead);
        grace.len = (n.tick - grace.tick).max(1).min(lead);
        grace.vel = (n.vel * scale).clamp(0.05, 1.0);
        if grace.tick < n.tick {
            extra.push(grace);
        }
    }
    notes.extend(extra);
    notes.sort_by_key(|n| n.tick);
}

pub fn tool_riff_arp(notes: &mut Vec<Note>, cfg: &ArpCfg) {
    if notes.is_empty() {
        return;
    }
    let mut base: Vec<u8> = notes.iter().map(|n| n.midi).collect();
    base.sort_unstable();
    base.dedup();
    let octs = cfg.octaves.clamp(1, 3);
    let mut seq = Vec::new();
    for o in 0..octs {
        for &m in &base {
            let shifted = m.saturating_add(o * 12);
            if shifted <= 108 {
                seq.push(shifted);
            }
        }
    }
    if seq.is_empty() {
        return;
    }
    match cfg.dir {
        ArpDir::Up => {}
        ArpDir::Down => seq.reverse(),
        ArpDir::UpDown if seq.len() > 2 => {
            let mut down = seq[1..seq.len() - 1].to_vec();
            down.reverse();
            seq.extend(down);
        }
        ArpDir::UpDown => {}
    }
    let step = cfg.step_ticks.max(1);
    let gate_len = ((step as f32 * cfg.gate.clamp(0.1, 1.5)).round() as u32).max(1);
    let max_t = cfg.max_ticks.max(step);
    let count = (max_t / step).max(1) as usize;
    notes.clear();
    for (i, &midi) in seq.iter().cycle().take(count).enumerate() {
        notes.push(Note::new(i as u32 * step, gate_len, midi, 0.85));
    }
}

pub fn tool_randomize(notes: &mut [Note], sel: &[usize], cfg: &RandCfg) {
    let mut x = cfg.seed | 1;
    let mut rnd_unit = move || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(37);
        ((x >> 33) as f32 / (u32::MAX as f32 * 0.5)) - 1.0
    };
    let mask = target_mask(notes.len(), sel);
    for (idx, n) in notes.iter_mut().enumerate() {
        if !mask[idx] {
            continue;
        }
        if cfg.pitch_range > 0 {
            let dp = (rnd_unit() * cfg.pitch_range as f32).round() as i32;
            n.midi = (n.midi as i32 + dp).clamp(21, 108) as u8;
        }
        if cfg.vel_amt > 0.0 {
            n.vel = (n.vel + rnd_unit() * cfg.vel_amt).clamp(0.05, 1.0);
        }
        if cfg.pan_amt > 0.0 {
            n.pan = (n.pan + rnd_unit() * cfg.pan_amt).clamp(-1.0, 1.0);
        }
    }
}

pub fn tool_flip(notes: &mut [Note], sel: &[usize], horizontal: bool, vertical: bool) {
    let mask = target_mask(notes.len(), sel);
    let mut min_t = u32::MAX;
    let mut max_end = 0u32;
    let mut min_m = u8::MAX;
    let mut max_m = 0u8;
    for (idx, n) in notes.iter().enumerate() {
        if mask[idx] {
            min_t = min_t.min(n.tick);
            max_end = max_end.max(n.tick.saturating_add(n.len));
            min_m = min_m.min(n.midi);
            max_m = max_m.max(n.midi);
        }
    }
    if min_t == u32::MAX {
        return;
    }
    for (idx, n) in notes.iter_mut().enumerate() {
        if !mask[idx] {
            continue;
        }
        if horizontal {
            let end = n.tick.saturating_add(n.len);
            n.tick = min_t + max_end.saturating_sub(end);
        }
        if vertical {
            let flipped = min_m as i32 + max_m as i32 - n.midi as i32;
            n.midi = flipped.clamp(21, 108) as u8;
        }
    }
}

/// FL Ctrl+L Quick Legato: extend each selected note to touch the next note start.
pub fn tool_legato(notes: &mut [Note], sel: &[usize], max_ticks: u32) {
    let mask = target_mask(notes.len(), sel);
    let mut order: Vec<usize> = (0..notes.len()).filter(|&i| mask[i]).collect();
    order.sort_by_key(|&i| notes[i].tick);
    for w in order.windows(2) {
        let cur = w[0];
        let next_tick = notes[w[1]].tick;
        if next_tick > notes[cur].tick {
            notes[cur].len = (next_tick - notes[cur].tick).max(1);
        }
    }
    if let Some(&last) = order.last()
        && max_ticks > notes[last].tick
    {
        notes[last].len = notes[last].len.max(max_ticks - notes[last].tick);
    }
}

/// Split note(s) crossing `tick` into two consecutive notes.
pub fn tool_slice_at(notes: &mut Vec<Note>, tick: u32, midi_filter: Option<u8>) -> bool {
    let mut added = Vec::new();
    for n in notes.iter_mut() {
        if let Some(m) = midi_filter
            && n.midi != m
        {
            continue;
        }
        let end = n.tick.saturating_add(n.len);
        if n.tick < tick && end > tick {
            let left_len = tick - n.tick;
            let right_len = end - tick;
            n.len = left_len;
            let mut right = *n;
            right.tick = tick;
            right.len = right_len;
            added.push(right);
        }
    }
    if added.is_empty() {
        return false;
    }
    notes.extend(added);
    notes.sort_by_key(|n| n.tick);
    true
}

/// FL Ctrl+B: duplicate selected notes (or all notes) immediately after selection end.
pub fn tool_duplicate_after(notes: &mut Vec<Note>, sel: &[usize], max_ticks: u32) -> Vec<usize> {
    let mask = target_mask(notes.len(), sel);
    let mut min_t = u32::MAX;
    let mut max_end = 0u32;
    for (idx, n) in notes.iter().enumerate() {
        if mask[idx] {
            min_t = min_t.min(n.tick);
            max_end = max_end.max(n.tick.saturating_add(n.len));
        }
    }
    if min_t == u32::MAX || max_end <= min_t {
        return Vec::new();
    }
    let span = max_end - min_t;
    let mut cloned = Vec::new();
    for (idx, n) in notes.iter().enumerate() {
        if mask[idx] {
            let mut c = *n;
            let nt = c.tick.saturating_add(span);
            if nt < max_ticks {
                c.tick = nt;
                c.len = c.len.min(max_ticks - nt).max(1);
                cloned.push(c);
            }
        }
    }
    let base = notes.len();
    let count = cloned.len();
    notes.extend(cloned);
    (base..base + count).collect()
}

pub fn tool_chord_ext(
    tick: u32,
    root: u8,
    kind: ChordKind,
    inversion: u8,
    len: u32,
    max_ticks: u32,
) -> Vec<Note> {
    let mut pitches: Vec<u8> = kind
        .intervals()
        .iter()
        .map(|iv| root.saturating_add(*iv).min(108))
        .collect();
    let inv = (inversion as usize).min(pitches.len().saturating_sub(1));
    for p in pitches.iter_mut().take(inv) {
        *p = p.saturating_add(12).min(108);
    }
    pitches.sort_unstable();
    pitches
        .into_iter()
        .map(|m| Note::new_in(tick, len.max(1), m, 0.85, max_ticks))
        .collect()
}

// ---------------------------------------------------------------------------
// Demo song
// ---------------------------------------------------------------------------

struct SynthCfg<'a> {
    name: &'a str,
    wave: WaveKind,
    base: u8,
    vol: f32,
    pan: f32,
    cutoff: f32,
    adsr: [f32; 4],
    insert: usize,
}

fn synth_ch(c: SynthCfg<'_>) -> Channel {
    Channel {
        name: c.name.into(),
        kind: ChannelKind::Synth { wave: c.wave },
        color: palette_color(c.insert),
        midi_base: c.base,
        volume: c.vol,
        pan: c.pan,
        cutoff: c.cutoff,
        q: 1.0,
        attack: c.adsr[0],
        decay: c.adsr[1],
        sustain: c.adsr[2],
        release: c.adsr[3],
        delay_send: 0.0,
        reverb_send: 0.15,
        mixer_insert: c.insert,
        ..Default::default()
    }
}

fn kit_ch(name: &str, piece: KitPiece, vol: f32, pan: f32, insert: usize) -> Channel {
    Channel {
        name: name.into(),
        kind: ChannelKind::Kit { piece },
        color: palette_color(insert),
        midi_base: 60,
        volume: vol,
        pan,
        cutoff: 12000.0,
        delay_send: 0.0,
        reverb_send: 0.1,
        mixer_insert: insert,
        ..Default::default()
    }
}

fn steps(pat: &mut Pattern, ch: usize, on: &[usize], midi: u8) {
    for &s in on {
        pat.notes[ch].push(Note::new(
            s as u32 * TICKS_PER_STEP,
            TICKS_PER_STEP,
            midi,
            0.9,
        ));
    }
}

fn default_channels() -> (Vec<Channel>, Vec<Pattern>) {
    let channels = vec![
        kit_ch("Kick", KitPiece::Kick, 1.0, 0.0, 1),
        kit_ch("Snare", KitPiece::Snare, 0.8, 0.0, 2),
        kit_ch("Clap", KitPiece::Clap, 0.6, 0.15, 3),
        kit_ch("Hat", KitPiece::Hat, 0.5, 0.2, 4),
        synth_ch(SynthCfg {
            name: "Bass",
            wave: WaveKind::Saw,
            base: 33,
            vol: 0.85,
            pan: -0.1,
            cutoff: 520.0,
            adsr: [0.008, 0.1, 0.7, 0.1],
            insert: 5,
        }),
        synth_ch(SynthCfg {
            name: "Lead",
            wave: WaveKind::Square,
            base: 69,
            vol: 0.55,
            pan: 0.0,
            cutoff: 2400.0,
            adsr: [0.01, 0.12, 0.6, 0.12],
            insert: 6,
        }),
        synth_ch(SynthCfg {
            name: "Pad",
            wave: WaveKind::Triangle,
            base: 57,
            vol: 0.5,
            pan: 0.0,
            cutoff: 1200.0,
            adsr: [0.2, 0.3, 0.7, 0.4],
            insert: 7,
        }),
        Channel {
            name: "Sampler".into(),
            kind: ChannelKind::Sampler { sample: None },
            color: palette_color(8),
            midi_base: 60,
            volume: 0.9,
            reverb_send: 0.1,
            mixer_insert: 8,
            ..Default::default()
        },
    ];
    let nch = channels.len();
    let patterns: Vec<Pattern> = (0..N_PATTERNS)
        .map(|i| {
            Pattern::empty_for_channels(
                format!("Pattern {}", i + 1),
                palette_color(i),
                nch,
                TimeSig::default(),
            )
        })
        .collect();
    (channels, patterns)
}

/// A brand-new project: named instruments, no notes, no arrangement.
/// This is what first-time users get instead of the demo groove.
pub fn new_empty() -> Song {
    let (channels, patterns) = default_channels();
    Song {
        save_version: SAVE_VERSION,
        title: "untitled".into(),
        bpm: 128.0,
        swing: 0.0,
        master_vol: 0.9,
        metronome: false,
        loop_song: true,
        delay_time: 0.32,
        delay_mix: 0.18,
        reverb_mix: 0.22,
        time_sig: TimeSig::default(),
        channels,
        patterns,
        playlist_tracks: (0..4).map(PlaylistTrack::new).collect(),
        clips: Vec::new(),
        mixer_tracks: default_mixer_tracks(),
        next_clip_id: 1,
        samples: Vec::new(),
        midi_mappings: Vec::new(),
        peak_controllers: Vec::new(),
        count_in_bars: 0,
        auto_quantize_rec: false,
        punch_in_tick: None,
        punch_out_tick: None,
    }
}

pub fn demo_song() -> Song {
    let (channels, mut patterns) = default_channels();

    // Pattern 0: full groove.
    steps(&mut patterns[0], 0, &[0, 4, 8, 12], 36);
    steps(&mut patterns[0], 1, &[4, 12], 40);
    steps(&mut patterns[0], 3, &[0, 2, 4, 6, 8, 10, 12, 14], 42);
    for (t, m) in [(0, 33), (16, 33), (32, 29), (48, 31)] {
        patterns[0].notes[4].push(Note::new(t, 8, m, 0.9));
    }
    for (t, l, m, v) in [
        (0, 8, 69, 0.9),
        (8, 4, 72, 0.9),
        (12, 4, 74, 0.85),
        (16, 8, 76, 0.9),
        (24, 4, 74, 0.85),
        (28, 4, 72, 0.8),
        (32, 8, 69, 0.9),
        (40, 4, 67, 0.85),
        (44, 4, 69, 0.9),
        (48, 4, 64, 0.85),
        (52, 4, 67, 0.8),
        (56, 8, 69, 0.9),
    ] {
        patterns[0].notes[5].push(Note::new(t, l, m, v));
    }
    patterns[0].notes[6].push(Note::new(0, 32, 57, 0.7));
    patterns[0].notes[6].push(Note::new(0, 32, 60, 0.6));
    patterns[0].notes[6].push(Note::new(0, 32, 64, 0.6));
    patterns[0].notes[6].push(Note::new(32, 32, 55, 0.7));
    patterns[0].notes[6].push(Note::new(32, 32, 59, 0.6));
    patterns[0].notes[6].push(Note::new(32, 32, 62, 0.6));

    // Pattern 1: drums only.
    steps(&mut patterns[1], 0, &[0, 4, 8, 12], 36);
    steps(&mut patterns[1], 1, &[4, 12], 40);
    steps(&mut patterns[1], 2, &[12], 39);
    steps(&mut patterns[1], 3, &[0, 2, 4, 6, 8, 10, 12, 14], 42);

    // Pattern 2: bass + lead, sparse drums.
    steps(&mut patterns[2], 0, &[0, 8], 36);
    steps(&mut patterns[2], 3, &[2, 6, 10, 14], 42);
    for (t, m) in [(0, 33), (16, 36), (32, 31), (48, 29)] {
        patterns[2].notes[4].push(Note::new(t, 8, m, 0.9));
    }
    for (t, l, m, v) in [
        (0, 8, 81, 0.9),
        (16, 8, 79, 0.85),
        (32, 8, 76, 0.9),
        (48, 8, 74, 0.9),
    ] {
        patterns[2].notes[5].push(Note::new(t, l, m, v));
    }

    // Pattern 3: break (hat + pad).
    steps(&mut patterns[3], 3, &[0, 2, 4, 6, 8, 10, 12, 14, 15], 42);
    patterns[3].notes[6].push(Note::new(0, 32, 57, 0.7));
    patterns[3].notes[6].push(Note::new(0, 32, 60, 0.6));
    patterns[3].notes[6].push(Note::new(0, 32, 64, 0.6));
    patterns[3].notes[6].push(Note::new(32, 32, 53, 0.7));
    patterns[3].notes[6].push(Note::new(32, 32, 57, 0.6));
    patterns[3].notes[6].push(Note::new(32, 32, 60, 0.6));

    let mut song = Song {
        save_version: SAVE_VERSION,
        title: "thunder demo".into(),
        bpm: 128.0,
        swing: 0.0,
        master_vol: 0.9,
        metronome: false,
        loop_song: true,
        delay_time: 0.32,
        delay_mix: 0.18,
        reverb_mix: 0.22,
        time_sig: TimeSig::default(),
        channels,
        patterns,
        playlist_tracks: (0..4).map(PlaylistTrack::new).collect(),
        clips: Vec::new(),
        mixer_tracks: default_mixer_tracks(),
        next_clip_id: 1,
        samples: Vec::new(),
        midi_mappings: Vec::new(),
        peak_controllers: Vec::new(),
        count_in_bars: 0,
        auto_quantize_rec: false,
        punch_in_tick: None,
        punch_out_tick: None,
    };
    for (bar, pat) in [0usize, 0, 1, 2, 0, 3, 2, 1].into_iter().enumerate() {
        song.add_pattern_clip(0, bar as u32 * TICKS_PATTERN, pat);
    }
    song
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_interp_midpoint() {
        let pts = vec![
            AutoPoint {
                tick: 0,
                value: 0.0,
            },
            AutoPoint {
                tick: 10,
                value: 1.0,
            },
        ];
        assert!((eval_auto(&pts, 5).unwrap() - 0.5).abs() < 1e-6);
        assert!((eval_auto(&pts, 0).unwrap() - 0.0).abs() < 1e-6);
        assert!((eval_auto(&pts, 99).unwrap() - 1.0).abs() < 1e-6);
        assert!(eval_auto(&[], 5).is_none());
    }

    #[test]
    fn quantize_snaps_to_grid() {
        let mut notes = vec![Note::new(5, 4, 60, 0.9), Note::new(13, 4, 62, 0.9)];
        tool_quantize_opts(
            &mut notes,
            &[],
            &QuantizeCfg {
                grid: 4.0,
                strength: 1.0,
                swing: 0.0,
                max_ticks: TICKS_PATTERN,
            },
        );
        assert_eq!(notes[0].tick, 4);
        assert_eq!(notes[1].tick, 12);
    }

    #[test]
    fn empty_project_has_no_notes() {
        let song = new_empty();
        assert_eq!(song.title, "untitled");
        assert!(song.clips.is_empty());
        assert!(
            song.patterns
                .iter()
                .all(|p| p.notes.iter().all(|ns| ns.is_empty()))
        );
        assert_eq!(song.channels.len(), 8);
    }

    #[test]
    fn step_toggle_roundtrip() {
        let mut song = demo_song();
        assert!(song.step_active(0, 0, 0));
        song.toggle_step(0, 0, 0);
        assert!(!song.step_active(0, 0, 0));
        song.toggle_step(0, 0, 0);
        assert!(song.step_active(0, 0, 0));
    }

    #[test]
    fn save_load_roundtrip() {
        let mut song = demo_song();
        song.patterns[0].bars = 2;
        song.add_channel(Channel {
            name: "Extra".into(),
            mixer_insert: 9,
            ..Default::default()
        });
        let path = std::env::temp_dir().join("thunder-test-v2.json");
        let path = path.to_str().unwrap();
        song.save(path).unwrap();
        let back = Song::load(path).unwrap();
        assert_eq!(back.title, song.title);
        assert_eq!(back.channels.len(), song.channels.len());
        assert_eq!(back.patterns.len(), song.patterns.len());
        assert_eq!(back.patterns[0].bars, 2);
        assert_eq!(back.clips.len(), song.clips.len());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn v1_project_migrates_to_v2() {
        let v1_json = serde_json::json!({
            "save_version": 1,
            "title": "legacy beat",
            "bpm": 124.0,
            "swing": 0.1,
            "master_vol": 0.9,
            "metronome": false,
            "loop_song": true,
            "delay_time": 0.3,
            "delay_mix": 0.15,
            "reverb_mix": 0.2,
            "channels": [
                {
                    "name": "Kick",
                    "kind": { "Kit": { "piece": "Kick" } },
                    "midi_base": 60,
                    "volume": 1.0,
                    "pan": 0.0,
                    "cutoff": 12000.0,
                    "q": 1.0,
                    "attack": 0.005,
                    "decay": 0.1,
                    "sustain": 0.8,
                    "release": 0.1,
                    "mute": false,
                    "solo": false,
                    "delay_send": 0.0,
                    "reverb_send": 0.1
                }
            ],
            "patterns": [
                {
                    "notes": [[{ "tick": 0, "len": 4, "midi": 36, "vel": 0.9 }]],
                    "auto_vol": [[]],
                    "auto_cut": [[]]
                }
            ],
            "arrangement": [0, -1, 0],
            "samples": []
        });
        let song = Song::from_json_str(&v1_json.to_string()).unwrap();
        assert_eq!(song.save_version, SAVE_VERSION);
        assert_eq!(song.title, "legacy beat");
        assert_eq!(song.clips.len(), 2);
        assert_eq!(song.clips[0].start_tick, 0);
        assert_eq!(song.clips[1].start_tick, 128);
        assert_eq!(song.channels[0].mixer_insert, 1);
    }

    #[test]
    fn mixer_routing_rejects_cycles_and_computes_pdc() {
        let mut song = new_empty();
        // Route Insert 1 -> Insert 2 (bus), remove Insert 1 -> Master.
        song.remove_mixer_route(1, 0);
        song.add_mixer_route(1, 2, 1.0, false).unwrap();
        assert!(song.add_mixer_route(2, 1, 1.0, false).is_err());
        assert!(song.add_mixer_route(0, 1, 1.0, false).is_err());

        song.mixer_tracks[2].latency_samples = 64;
        let pdc = song.compute_pdc();
        // Insert 2 -> Master arrives at 64 samples; Insert 3 -> Master arrives at 0 samples.
        // So route Insert 3 -> Master must get 64 samples of PDC delay.
        assert_eq!(pdc.total_out[2], 64);
        assert_eq!(pdc.route_delay[3][0], (0, 64));
        assert_eq!(pdc.route_delay[2][0], (0, 0));
    }

    #[test]
    fn dynamic_channels_and_patterns_stay_aligned() {
        let mut song = new_empty();
        let n0 = song.channels.len();
        let ci = song.add_channel(Channel::default());
        assert_eq!(ci, n0);
        assert!(song.patterns.iter().all(|p| p.notes.len() == n0 + 1));
        assert!(song.move_channel(ci, 0));
        assert_eq!(song.channels[0].name, "Channel");
        assert!(song.delete_channel(0));
        assert!(song.patterns.iter().all(|p| p.notes.len() == n0));

        let pi = song.add_pattern(Some("Verse".into()));
        assert_eq!(song.patterns[pi].name, "Verse");
        let clip_id = song.add_pattern_clip(0, 0, pi);
        let uniq = song.make_clip_unique(clip_id).unwrap();
        assert_ne!(uniq, pi);
        assert!(song.delete_pattern(uniq));
    }

    #[test]
    fn arp_produces_16_steps() {
        let mut notes = vec![Note::new(0, 4, 60, 0.9), Note::new(16, 4, 67, 0.9)];
        tool_riff_arp(
            &mut notes,
            &ArpCfg {
                step_ticks: TICKS_PER_STEP,
                gate: 0.9,
                octaves: 1,
                dir: ArpDir::Up,
                max_ticks: TICKS_PATTERN,
            },
        );
        assert_eq!(notes.len(), 16);
        assert!(notes.windows(2).all(|w| w[0].tick <= w[1].tick));
    }

    #[test]
    fn strum_spreads_chord_cluster() {
        let mut notes = vec![
            Note::new(8, 4, 60, 0.9),
            Note::new(8, 4, 64, 0.9),
            Note::new(8, 4, 67, 0.9),
            Note::new(24, 4, 69, 0.9),
        ];
        tool_strum_opts(
            &mut notes,
            &[],
            &StrumCfg {
                amount: 2,
                vel_slope: 0.0,
                preserve_end: false,
                max_ticks: TICKS_PATTERN,
            },
        );
        let starts: Vec<u32> = notes.iter().map(|n| n.tick).collect();
        assert_eq!(starts, vec![8, 10, 12, 24]);
    }

    #[test]
    fn chord_builds_triad() {
        let maj = tool_chord_ext(0, 60, ChordKind::Maj, 0, TICKS_PER_STEP, TICKS_PATTERN);
        assert_eq!(
            maj.iter().map(|n| n.midi).collect::<Vec<_>>(),
            vec![60, 64, 67]
        );
        let min = tool_chord_ext(0, 60, ChordKind::Min, 0, TICKS_PER_STEP, TICKS_PATTERN);
        assert_eq!(
            min.iter().map(|n| n.midi).collect::<Vec<_>>(),
            vec![60, 63, 67]
        );
    }

    #[test]
    fn humanize_keeps_shape() {
        let mut notes = vec![Note::new(16, 4, 60, 0.9), Note::new(32, 4, 64, 0.9)];
        tool_randomize(
            &mut notes,
            &[],
            &RandCfg {
                seed: 12345,
                pitch_range: 0,
                vel_amt: 0.15,
                pan_amt: 0.1,
            },
        );
        assert_eq!(notes.len(), 2);
        for n in &notes {
            assert!(n.tick < TICKS_PATTERN);
            assert!((0.05..=1.0).contains(&n.vel));
        }
    }

    #[test]
    fn song_clone_shares_sample_audio() {
        let mut song = new_empty();
        song.samples.push(SampleRef::from_mono(
            "tone.wav",
            "tone.wav",
            vec![0.1, -0.1],
            44100.0,
        ));

        let copy = song.clone();
        assert!(Arc::ptr_eq(&song.samples[0].data, &copy.samples[0].data));
        assert!(!song.samples[0].peaks.is_empty());
    }

    #[test]
    fn sample_dsp_ops_and_fx_slots_work() {
        let mut smp = SampleRef::from_stereo(
            "st.wav",
            "st.wav",
            vec![0.6, 0.2, 0.4, 0.2],
            vec![0.2, 0.4, 0.6, 0.2],
            44100.0,
        );
        assert!(smp.is_stereo());
        smp.remove_dc_in_place();
        let mean: f32 = smp.data.iter().sum::<f32>() / smp.data.len() as f32;
        assert!(mean.abs() < 1e-5);
        smp.normalize_in_place();
        let peak = smp.data.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        assert!(peak > 0.3);
        smp.reverse_in_place();

        let mut song = new_empty();
        for _ in 0..MAX_FX_SLOTS {
            assert!(
                song.mixer_tracks[1]
                    .add_fx(EffectSlot::new(FxKind::ParametricEq(EqCfg::default())))
            );
        }
        assert!(
            !song.mixer_tracks[1].add_fx(EffectSlot::new(FxKind::Reverb(ReverbFxCfg::default())))
        );
        assert!(song.mixer_tracks[1].move_fx(0, 2));

        let lim = CompCfg {
            limiter: true,
            lookahead_samples: 32,
            ..Default::default()
        };
        song.mixer_tracks[2].add_fx(EffectSlot::new(FxKind::Compressor(lim)));
        assert_eq!(song.mixer_tracks[2].effective_latency_samples(), 32);

        let pads = default_drum_pads();
        assert_eq!(pads.len(), 16);
        song.channels.push(Channel {
            name: "Drums".into(),
            kind: ChannelKind::DrumRack { pads },
            ..Default::default()
        });
        let json = serde_json::to_string(&song).unwrap();
        let back = Song::from_json_str(&json).unwrap();
        assert_eq!(back.mixer_tracks[1].fx.len(), MAX_FX_SLOTS);
        assert_eq!(back.channels.len(), song.channels.len());
    }

    #[test]
    fn piano_roll_tools_legato_slice_duplicate_flip_chord() {
        let mut notes = vec![Note::new(0, 4, 60, 0.9), Note::new(16, 4, 64, 0.8)];
        tool_legato(&mut notes, &[0, 1], 64);
        assert_eq!(notes[0].len, 16);
        assert_eq!(notes[1].len, 48);

        assert!(tool_slice_at(&mut notes, 8, None));
        assert_eq!(notes.len(), 3);
        assert_eq!((notes[0].tick, notes[0].len), (0, 8));
        assert_eq!((notes[1].tick, notes[1].len), (8, 8));

        let dup_idx = tool_duplicate_after(&mut notes, &[0], 64);
        assert_eq!(dup_idx, vec![3]);
        assert_eq!(notes[3].tick, 8);

        tool_flip(&mut notes, &[0, 2], true, true);
        assert_eq!(notes[0].midi, 64);
        assert_eq!(notes[2].midi, 60);

        let inv = tool_chord_ext(0, 60, ChordKind::Maj7, 1, 8, 64);
        assert_eq!(
            inv.iter().map(|n| n.midi).collect::<Vec<_>>(),
            vec![64, 67, 71, 72]
        );
        assert!(ScaleKind::Blues.contains(6, 0));
        assert!(!ScaleKind::Dorian.contains(1, 0));

        let mut fl = vec![Note::new(8, 4, 60, 0.9)];
        tool_flam(&mut fl, &[], 2, 0.5);
        assert_eq!(fl.len(), 2);
        assert_eq!(fl[0].tick, 6);
        assert!((fl[0].vel - 0.45).abs() < 1e-4);
    }

    #[test]
    fn phase5_auto_curves_midi_mapping_and_edison_ops() {
        let hold_pts = vec![
            AutoClipPoint {
                tick: 0,
                value: 0.2,
                tension: 0.0,
                curve: CurveMode::Hold,
            },
            AutoClipPoint {
                tick: 10,
                value: 0.8,
                tension: 0.0,
                curve: CurveMode::Linear,
            },
        ];
        assert!((eval_auto_clip(&hold_pts, 5.0).unwrap() - 0.2).abs() < 1e-5);
        assert!((eval_auto_clip(&hold_pts, 10.0).unwrap() - 0.8).abs() < 1e-5);

        let smooth_pts = vec![
            AutoClipPoint {
                tick: 0,
                value: 0.0,
                tension: 0.0,
                curve: CurveMode::Smooth,
            },
            AutoClipPoint {
                tick: 10,
                value: 1.0,
                tension: 0.0,
                curve: CurveMode::Smooth,
            },
        ];
        assert!((eval_auto_clip(&smooth_pts, 5.0).unwrap() - 0.5).abs() < 1e-4);
        assert!(eval_auto_clip(&smooth_pts, 2.5).unwrap() < 0.25);

        let exp_pts = vec![
            AutoClipPoint {
                tick: 0,
                value: 0.0,
                tension: 0.5,
                curve: CurveMode::Exp,
            },
            AutoClipPoint {
                tick: 10,
                value: 1.0,
                tension: 0.0,
                curve: CurveMode::Exp,
            },
        ];
        assert!(eval_auto_clip(&exp_pts, 5.0).unwrap() < 0.2);

        let map_inv = MidiMapping {
            cc: 74,
            channel: None,
            target: AutoTarget::ChannelVol(0),
            curve: MapCurve::Inverted,
            min: 0.0,
            max: 1.0,
        };
        assert!((map_cc_value(0, &map_inv) - 1.0).abs() < 1e-5);
        assert!((map_cc_value(127, &map_inv) - 0.0).abs() < 1e-5);

        let map_log = MidiMapping {
            cc: 74,
            channel: Some(1),
            target: AutoTarget::ChannelCut(0),
            curve: MapCurve::Log,
            min: 100.0,
            max: 10000.0,
        };
        let mid_cut = map_cc_value(64, &map_log);
        assert!((800.0..=1300.0).contains(&mid_cut));

        let mut song = new_empty();
        song.midi_mappings.push(map_log);
        song.peak_controllers.push(PeakController::default());
        song.count_in_bars = 1;
        song.auto_quantize_rec = true;
        song.punch_in_tick = Some(16);
        song.punch_out_tick = Some(64);
        let json = serde_json::to_string(&song).unwrap();
        let back = Song::from_json_str(&json).unwrap();
        assert_eq!(back.midi_mappings.len(), 1);
        assert_eq!(back.peak_controllers.len(), 1);
        assert_eq!(back.count_in_bars, 1);
        assert!(back.auto_quantize_rec);
        assert_eq!(back.punch_in_tick, Some(16));

        let mut pcm = vec![0.0f32; 2000];
        pcm[100..200].fill(0.8);
        pcm[1100..1200].fill(0.9);
        let mut smp = SampleRef::from_mono("take.wav", "take.wav", pcm, 44100.0);
        let slices = smp.detect_transients(0.3, 400);
        assert!(slices.len() >= 2);
        let slice0 = smp.extract_slice("slice0.wav", 100, 400);
        assert_eq!(slice0.data.len(), 300);
        smp.fade_in_range_in_place(0.0, 0.5);
        smp.fade_out_range_in_place(0.5, 1.0);
        smp.trim_range_in_place(0.05, 0.6);
        assert_eq!(smp.data.len(), 1100);
    }

    #[test]
    fn phase7_clap_serde_and_pdc_alignment() {
        let mut song = new_empty();
        let zero_ch = song.add_channel(Channel {
            name: "ZeroSynth".into(),
            kind: ChannelKind::Synth {
                wave: WaveKind::Saw,
            },
            mixer_insert: 1,
            ..Default::default()
        });
        let clap_ch = song.add_channel(Channel {
            name: "Stub Synth".into(),
            kind: ChannelKind::ClapPlugin(ClapPluginCfg {
                path: "/tmp/stub.clap".into(),
                plugin_id: "org.cotanjant.thunder.test_stub".into(),
                name: "Stub Synth".into(),
                reported_latency: 32,
                state_b64: "AAAA".into(),
                params: vec![(1, 0.75)],
                rt_id: next_clap_rt_id(),
            }),
            mixer_insert: 1,
            ..Default::default()
        });
        assert!(
            song.mixer_tracks[1].add_fx(EffectSlot::new(FxKind::ClapPlugin(ClapPluginCfg {
                path: "/tmp/stub.clap".into(),
                plugin_id: "org.cotanjant.thunder.test_stub".into(),
                name: "Stub FX".into(),
                reported_latency: 16,
                state_b64: String::new(),
                params: Vec::new(),
                rt_id: next_clap_rt_id(),
            })))
        );

        let json = serde_json::to_string(&song).unwrap();
        let back = Song::from_json_str(&json).unwrap();
        assert!(
            matches!(back.channels[clap_ch].kind, ChannelKind::ClapPlugin(ref c) if c.reported_latency == 32 && c.params == vec![(1, 0.75)])
        );
        assert!(
            matches!(back.mixer_tracks[1].fx[0].kind, FxKind::ClapPlugin(ref c) if c.reported_latency == 16)
        );

        let pdc = back.compute_pdc();
        assert_eq!(pdc.ch_delay[clap_ch], 0);
        assert_eq!(pdc.ch_delay[zero_ch], 32);
    }

    #[test]
    fn set_step_sets_and_clears_steps_without_duplicates() {
        let mut song = new_empty();
        assert!(song.set_step(0, 0, 2, true));
        assert!(!song.set_step(0, 0, 2, true));
        assert_eq!(song.patterns[0].notes[0].len(), 1);
        assert_eq!(song.patterns[0].notes[0][0].tick, 2 * TICKS_PER_STEP);

        assert!(song.set_step(0, 0, 2, false));
        assert!(!song.set_step(0, 0, 2, false));
        assert!(song.patterns[0].notes[0].is_empty());
        assert!(!song.set_step(99, 0, 0, true));
    }
}
