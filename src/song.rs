//! Song model: FL-style dynamic channels, variable-length patterns with
//! time signatures, multi-track playlist clips, routable mixer insert DAG
//! with PDC calculation, and versioned JSON save/load (v1 -> v2 migration).

use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const TICKS_PER_STEP: u32 = 4; // 64th-note resolution
pub const STEPS: usize = 16;
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelKind {
    Synth { wave: WaveKind },
    Kit { piece: KitPiece },
    Sampler { sample: Option<usize> },
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// One user sample. PCM is reloaded from `path` on project load
/// (never stored in the JSON).
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SampleRef {
    pub name: String,
    pub path: String,
    #[serde(skip)]
    pub data: Arc<[f32]>,
    #[serde(skip)]
    pub sr: f32,
}

impl PartialEq for SampleRef {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.path == other.path && self.sr == other.sr
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub tick: u32,
    pub len: u32,
    pub midi: u8,
    pub vel: f32,
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub fine_pitch: f32,
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
                // Kit/sampler channels still get a pool so live preview works.
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

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoTarget {
    ChannelVol(usize),
    ChannelCut(usize),
    InsertVol(usize),
    MasterVol,
}

impl AutoTarget {
    pub fn label(&self) -> String {
        match self {
            AutoTarget::ChannelVol(ch) => format!("Ch {} Vol", ch + 1),
            AutoTarget::ChannelCut(ch) => format!("Ch {} Cut", ch + 1),
            AutoTarget::InsertVol(ins) => {
                if *ins == 0 {
                    "Master Vol".into()
                } else {
                    format!("Ins {ins} Vol")
                }
            }
            AutoTarget::MasterVol => "Master Vol".into(),
        }
    }

    pub fn range(&self) -> (f32, f32) {
        match self {
            AutoTarget::ChannelVol(_) | AutoTarget::InsertVol(_) | AutoTarget::MasterVol => {
                (0.0, 1.5)
            }
            AutoTarget::ChannelCut(_) => (80.0, 16000.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AutoClipPoint {
    pub tick: u32,
    pub value: f32,
    #[serde(default)]
    pub tension: f32,
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
        }
    }
}

pub struct PdcPlan {
    pub order: Vec<usize>,
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
        let mid = match target {
            AutoTarget::ChannelCut(_) => 2000.0,
            _ => (lo + hi) * 0.5,
        };
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
                        value: mid,
                        tension: 0.0,
                    },
                    AutoClipPoint {
                        tick: len,
                        value: mid,
                        tension: 0.0,
                    },
                ],
            },
        });
        id
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

    /// Automatic Plugin Delay Compensation across parallel routed mixer paths.
    pub fn compute_pdc(&self) -> PdcPlan {
        let n = self.mixer_tracks.len();
        let order = self.mixer_topo_order();
        let mut incoming_non_sc: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
        for u in 0..n {
            for (ri, r) in self.mixer_tracks[u].routes.iter().enumerate() {
                if !r.sidechain_only && r.target < n {
                    incoming_non_sc[r.target].push((u, ri));
                }
            }
        }
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
            direct_delay[v] = max_in;
            for &(u, ri) in &incoming_non_sc[v] {
                route_delay[u][ri].1 = max_in.saturating_sub(total_out[u]);
            }
            total_out[v] = max_in.saturating_add(self.mixer_tracks[v].latency_samples);
        }

        PdcPlan {
            order,
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
        if t_local >= t0 && t_local <= t1 {
            let span = (t1 - t0).max(1e-4);
            let u = ((t_local - t0) / span).clamp(0.0, 1.0);
            let ten = w[0].tension.clamp(-1.0, 1.0);
            let shaped = if ten.abs() < 1e-3 {
                u
            } else if ten > 0.0 {
                u.powf(1.0 + ten * 3.0)
            } else {
                1.0 - (1.0 - u).powf(1.0 + (-ten) * 3.0)
            };
            return Some(w[0].value + (w[1].value - w[0].value) * shaped);
        }
    }
    points.last().map(|p| p.value)
}

// ---------------------------------------------------------------------------
// Note editing tools (pure functions, unit-testable)
// ---------------------------------------------------------------------------

pub fn tool_quantize_in(notes: &mut [Note], grid: u32, max_ticks: u32) {
    let grid = grid.max(1);
    for n in notes.iter_mut() {
        n.tick = (n.tick + grid / 2) / grid * grid;
        n.tick = n.tick.min(max_ticks.saturating_sub(1));
    }
    notes.sort_by_key(|n| n.tick);
}

pub fn tool_humanize_in(notes: &mut [Note], seed: u64, max_ticks: u32) {
    let mut x = seed | 1;
    let mut rnd = move || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(37);
        (x >> 33) as u32
    };
    let cap = max_ticks.saturating_sub(1) as i32;
    for n in notes.iter_mut() {
        let dt = (rnd() % 5) as i32 - 2;
        n.tick = (n.tick as i32 + dt).clamp(0, cap) as u32;
        let dv = ((rnd() % 17) as f32 - 8.0) / 100.0;
        n.vel = (n.vel + dv).clamp(0.05, 1.0);
    }
    notes.sort_by_key(|n| n.tick);
}

/// Spread chord clusters (notes within 2 ticks) by `amount` ticks each.
pub fn tool_strum_in(notes: &mut [Note], amount: u32, max_ticks: u32) {
    notes.sort_by_key(|n| n.tick);
    let mut i = 0;
    let cap = max_ticks.saturating_sub(1);
    while i < notes.len() {
        let t = notes[i].tick;
        let mut j = i;
        while j < notes.len() && notes[j].tick.saturating_sub(t) <= 2 {
            j += 1;
        }
        for (k, n) in notes[i..j].iter_mut().enumerate() {
            n.tick = (t + k as u32 * amount).min(cap);
        }
        i = j.max(i + 1);
    }
}

/// Rewrite notes as an ascending 16th-note arpeggio cycling their pitches.
pub fn tool_arpeggiate(notes: &mut Vec<Note>) {
    if notes.is_empty() {
        return;
    }
    let mut pitches: Vec<u8> = notes.iter().map(|n| n.midi).collect();
    pitches.sort_unstable();
    pitches.dedup();
    notes.clear();
    for (i, p) in pitches.iter().cycle().take(STEPS).enumerate() {
        notes.push(Note::new(
            i as u32 * TICKS_PER_STEP,
            TICKS_PER_STEP,
            *p,
            0.85,
        ));
    }
}

pub fn tool_chord(tick: u32, root: u8, minor: bool) -> Vec<Note> {
    let third = if minor { 3 } else { 4 };
    [0, third, 7]
        .iter()
        .map(|iv| Note::new(tick, TICKS_PER_STEP * 4, root.saturating_add(*iv), 0.85))
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
        tool_quantize_in(&mut notes, 4, TICKS_PATTERN);
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
        tool_arpeggiate(&mut notes);
        assert_eq!(notes.len(), STEPS);
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
        tool_strum_in(&mut notes, 2, TICKS_PATTERN);
        let starts: Vec<u32> = notes.iter().map(|n| n.tick).collect();
        assert_eq!(starts, vec![8, 10, 12, 24]);
    }

    #[test]
    fn chord_builds_triad() {
        let maj = tool_chord(0, 60, false);
        assert_eq!(
            maj.iter().map(|n| n.midi).collect::<Vec<_>>(),
            vec![60, 64, 67]
        );
        let min = tool_chord(0, 60, true);
        assert_eq!(
            min.iter().map(|n| n.midi).collect::<Vec<_>>(),
            vec![60, 63, 67]
        );
    }

    #[test]
    fn humanize_keeps_shape() {
        let mut notes = vec![Note::new(16, 4, 60, 0.9), Note::new(32, 4, 64, 0.9)];
        tool_humanize_in(&mut notes, 12345, TICKS_PATTERN);
        assert_eq!(notes.len(), 2);
        for n in &notes {
            assert!(n.tick < TICKS_PATTERN);
            assert!((0.05..=1.0).contains(&n.vel));
        }
    }

    #[test]
    fn song_clone_shares_sample_audio() {
        let mut song = new_empty();
        song.samples.push(SampleRef {
            name: "tone.wav".into(),
            path: "tone.wav".into(),
            data: vec![0.1, -0.1].into(),
            sr: 44100.0,
        });

        let copy = song.clone();
        assert!(Arc::ptr_eq(&song.samples[0].data, &copy.samples[0].data));
    }
}
