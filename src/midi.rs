//! Hardware MIDI I/O (midir), MIDI Learn, Piano Roll MIDI recording,
//! and cpal live audio input capture.

use crate::song::{AutoTarget, MapCurve, MidiMapping, Note, Song, TICKS_PER_STEP, map_cc_value};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MidiMsg {
    NoteOn { ch: u8, note: u8, vel: f32 },
    NoteOff { ch: u8, note: u8 },
    Cc { ch: u8, cc: u8, val: u8 },
    PitchBend { ch: u8, norm: f32 },
}

/// Parse a raw MIDI 1.0 channel message into a [`MidiMsg`].
pub fn parse_midi_bytes(bytes: &[u8]) -> Option<MidiMsg> {
    if bytes.is_empty() {
        return None;
    }
    let status = bytes[0];
    if !(0x80..0xF0).contains(&status) {
        return None;
    }
    let kind = status & 0xF0;
    let ch = status & 0x0F;
    match kind {
        0x80 => {
            let note = *bytes.get(1)?;
            Some(MidiMsg::NoteOff {
                ch,
                note: note & 0x7F,
            })
        }
        0x90 => {
            let note = *bytes.get(1)? & 0x7F;
            let raw_vel = *bytes.get(2)? & 0x7F;
            if raw_vel == 0 {
                Some(MidiMsg::NoteOff { ch, note })
            } else {
                Some(MidiMsg::NoteOn {
                    ch,
                    note,
                    vel: (raw_vel as f32 / 127.0).clamp(0.05, 1.0),
                })
            }
        }
        0xB0 => {
            let cc = *bytes.get(1)? & 0x7F;
            let val = *bytes.get(2)? & 0x7F;
            Some(MidiMsg::Cc { ch, cc, val })
        }
        0xE0 => {
            let lsb = (*bytes.get(1)? & 0x7F) as i32;
            let msb = (*bytes.get(2)? & 0x7F) as i32;
            let val14 = (msb << 7) | lsb;
            let norm = ((val14 - 8192) as f32 / 8192.0).clamp(-1.0, 1.0);
            Some(MidiMsg::PitchBend { ch, norm })
        }
        _ => None,
    }
}

pub fn list_midi_inputs() -> Vec<String> {
    let Ok(midi_in) = midir::MidiInput::new("thunder-probe-in") else {
        return Vec::new();
    };
    midi_in
        .ports()
        .iter()
        .filter_map(|p| midi_in.port_name(p).ok())
        .collect()
}

pub fn list_midi_outputs() -> Vec<String> {
    let Ok(midi_out) = midir::MidiOutput::new("thunder-probe-out") else {
        return Vec::new();
    };
    midi_out
        .ports()
        .iter()
        .filter_map(|p| midi_out.port_name(p).ok())
        .collect()
}

pub fn list_audio_inputs() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devs) = host.input_devices() else {
        return Vec::new();
    };
    devs.filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
        .collect()
}

struct MidiConnPair {
    rt_tx: Option<rtrb::Producer<MidiMsg>>,
    ui_tx: rtrb::Producer<MidiMsg>,
}

pub struct MidiService {
    pub in_ports: Vec<String>,
    pub out_ports: Vec<String>,
    pub selected_in: Option<usize>,
    pub selected_out: Option<usize>,
    in_conn: Option<midir::MidiInputConnection<MidiConnPair>>,
    out_conn: Option<midir::MidiOutputConnection>,
    rt_tx_parked: Option<rtrb::Producer<MidiMsg>>,
    ui_tx_parked: Option<rtrb::Producer<MidiMsg>>,
    pub ui_rx: rtrb::Consumer<MidiMsg>,
}

impl Default for MidiService {
    fn default() -> Self {
        let (ui_tx, ui_rx) = rtrb::RingBuffer::new(512);
        Self {
            in_ports: Vec::new(),
            out_ports: Vec::new(),
            selected_in: None,
            selected_out: None,
            in_conn: None,
            out_conn: None,
            rt_tx_parked: None,
            ui_tx_parked: Some(ui_tx),
            ui_rx,
        }
    }
}

impl MidiService {
    pub fn set_audio_tx(&mut self, tx: rtrb::Producer<MidiMsg>) {
        if self.in_conn.is_some() {
            let idx = self.selected_in;
            self.disconnect_in();
            self.rt_tx_parked = Some(tx);
            if let Some(i) = idx {
                let _ = self.connect_in(i);
            }
        } else {
            self.rt_tx_parked = Some(tx);
        }
    }

    pub fn refresh(&mut self) {
        self.in_ports = list_midi_inputs();
        self.out_ports = list_midi_outputs();
    }

    pub fn disconnect_in(&mut self) {
        if let Some(conn) = self.in_conn.take() {
            let (_, pair) = conn.close();
            if pair.rt_tx.is_some() {
                self.rt_tx_parked = pair.rt_tx;
            }
            self.ui_tx_parked = Some(pair.ui_tx);
        }
        self.selected_in = None;
    }

    pub fn connect_in(&mut self, port_idx: usize) -> Result<String, String> {
        self.disconnect_in();
        let midi_in = midir::MidiInput::new("thunder-in").map_err(|e| format!("{e}"))?;
        let ports = midi_in.ports();
        let port = ports
            .get(port_idx)
            .ok_or_else(|| "MIDI input port out of range".to_string())?;
        let name = midi_in
            .port_name(port)
            .unwrap_or_else(|_| format!("Port {port_idx}"));
        let pair = MidiConnPair {
            rt_tx: self.rt_tx_parked.take(),
            ui_tx: self
                .ui_tx_parked
                .take()
                .ok_or_else(|| "MIDI UI ring unavailable".to_string())?,
        };
        let conn = midi_in
            .connect(
                port,
                "thunder-midi-in",
                move |_stamp, msg, pair| {
                    if let Some(parsed) = parse_midi_bytes(msg) {
                        if let Some(rt) = pair.rt_tx.as_mut() {
                            let _ = rt.push(parsed);
                        }
                        let _ = pair.ui_tx.push(parsed);
                    }
                },
                pair,
            )
            .map_err(|e| format!("{e}"))?;
        self.in_conn = Some(conn);
        self.selected_in = Some(port_idx);
        Ok(name)
    }

    pub fn connect_out(&mut self, port_idx: usize) -> Result<String, String> {
        self.out_conn = None;
        self.selected_out = None;
        let midi_out = midir::MidiOutput::new("thunder-out").map_err(|e| format!("{e}"))?;
        let ports = midi_out.ports();
        let port = ports
            .get(port_idx)
            .ok_or_else(|| "MIDI output port out of range".to_string())?;
        let name = midi_out
            .port_name(port)
            .unwrap_or_else(|_| format!("Out {port_idx}"));
        let conn = midi_out
            .connect(port, "thunder-midi-out")
            .map_err(|e| format!("{e}"))?;
        self.out_conn = Some(conn);
        self.selected_out = Some(port_idx);
        Ok(name)
    }

    pub fn send_note_on(&mut self, ch: u8, note: u8, vel: f32) {
        if let Some(conn) = self.out_conn.as_mut() {
            let v = (vel * 127.0).round().clamp(1.0, 127.0) as u8;
            let _ = conn.send(&[0x90 | (ch & 0x0F), note & 0x7F, v]);
        }
    }

    pub fn send_note_off(&mut self, ch: u8, note: u8) {
        if let Some(conn) = self.out_conn.as_mut() {
            let _ = conn.send(&[0x80 | (ch & 0x0F), note & 0x7F, 0]);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum MidiRecMode {
    #[default]
    Overdub,
    Replace,
    StepInput,
}

impl MidiRecMode {
    pub const ALL: [MidiRecMode; 3] = [
        MidiRecMode::Overdub,
        MidiRecMode::Replace,
        MidiRecMode::StepInput,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            MidiRecMode::Overdub => "Overdub",
            MidiRecMode::Replace => "Replace",
            MidiRecMode::StepInput => "Step",
        }
    }
}

#[derive(Clone, Copy)]
pub struct HeldMidiNote {
    pub midi: u8,
    pub start_tick: u32,
    pub vel: f32,
}

pub struct MidiRecorder {
    pub armed: bool,
    pub mode: MidiRecMode,
    pub step_cursor: u32,
    pub step_len: u32,
    pub held: Vec<HeldMidiNote>,
    pub replaced_pass: bool,
    pub mod_wheel: f32,
    pub sustain: bool,
    pub pitch_bend: f32,
}

impl Default for MidiRecorder {
    fn default() -> Self {
        Self {
            armed: false,
            mode: MidiRecMode::Overdub,
            step_cursor: 0,
            step_len: TICKS_PER_STEP,
            held: Vec::new(),
            replaced_pass: false,
            mod_wheel: 0.0,
            sustain: false,
            pitch_bend: 0.0,
        }
    }
}

#[derive(Default)]
pub struct MidiLearnState {
    pub armed_target: Option<AutoTarget>,
    pub pending_curve: MapCurve,
    pub last_cc: Option<(u8, u8, u8)>,
}

impl MidiLearnState {
    pub fn process_cc(&mut self, song: &mut Song, ch: u8, cc: u8, val: u8) -> bool {
        self.last_cc = Some((ch, cc, val));
        if let Some(target) = self.armed_target.take() {
            let (min, max) = target.range();
            let mapping = MidiMapping {
                cc,
                channel: None,
                target,
                curve: self.pending_curve,
                min,
                max,
            };
            song.midi_mappings
                .retain(|m| !(m.target == target || (m.cc == cc && m.channel == mapping.channel)));
            song.midi_mappings.push(mapping);
            let mapped = map_cc_value(val, &mapping);
            song.apply_target_value(target, mapped);
            return true;
        }
        let mut changed = false;
        let matches: Vec<MidiMapping> = song
            .midi_mappings
            .iter()
            .copied()
            .filter(|m| m.cc == cc && m.channel.is_none_or(|c| c == ch))
            .collect();
        for m in matches {
            let mapped = map_cc_value(val, &m);
            if song.apply_target_value(m.target, mapped) {
                changed = true;
            }
        }
        changed
    }
}

fn snap_tick(tick: u32, grid: u32, pat_len: u32) -> u32 {
    if grid <= 1 {
        return tick.min(pat_len.saturating_sub(1));
    }
    let half = grid / 2;
    let snapped = ((tick + half) / grid) * grid;
    snapped.min(pat_len.saturating_sub(1))
}

#[derive(Clone, Copy)]
pub struct MidiRecCtx {
    pub pat: usize,
    pub ch: usize,
    pub cur_tick: u32,
    pub playing: bool,
    pub snap_grid: u32,
}

impl MidiRecorder {
    pub fn reset_pass(&mut self) {
        self.held.clear();
        self.replaced_pass = false;
    }

    /// Record or update live MIDI state on `song.patterns[pat].notes[ch]`.
    /// Returns `true` when pattern notes were modified.
    pub fn handle_msg(&mut self, song: &mut Song, msg: MidiMsg, ctx: MidiRecCtx) -> bool {
        let MidiRecCtx {
            pat,
            ch,
            cur_tick,
            playing,
            snap_grid,
        } = ctx;
        match msg {
            MidiMsg::Cc { cc: 1, val, .. } => {
                self.mod_wheel = val as f32 / 127.0;
                false
            }
            MidiMsg::Cc { cc: 64, val, .. } => {
                self.sustain = val >= 64;
                false
            }
            MidiMsg::PitchBend { norm, .. } => {
                self.pitch_bend = norm;
                false
            }
            MidiMsg::Cc { .. } => false,
            MidiMsg::NoteOn { note, vel, .. } => {
                if !self.armed {
                    return false;
                }
                let Some(p) = song.patterns.get_mut(pat) else {
                    return false;
                };
                if ch >= p.notes.len() {
                    return false;
                }
                let pat_len = p.len_ticks().max(1);
                if self.mode == MidiRecMode::StepInput {
                    let step = self.step_len.max(1);
                    let t = self.step_cursor % pat_len;
                    p.notes[ch].push(Note::new_in(t, step, note, vel, pat_len));
                    self.step_cursor = (t + step) % pat_len;
                    return true;
                }
                if !playing {
                    return false;
                }
                if self.mode == MidiRecMode::Replace && !self.replaced_pass {
                    p.notes[ch].clear();
                    self.replaced_pass = true;
                }
                let tick = if song.auto_quantize_rec {
                    snap_tick(cur_tick % pat_len, snap_grid.max(1), pat_len)
                } else {
                    cur_tick % pat_len
                };
                self.held.retain(|h| h.midi != note);
                self.held.push(HeldMidiNote {
                    midi: note,
                    start_tick: tick,
                    vel,
                });
                false
            }
            MidiMsg::NoteOff { note, .. } => {
                let Some(idx) = self.held.iter().position(|h| h.midi == note) else {
                    return false;
                };
                let held = self.held.remove(idx);
                if !self.armed || self.mode == MidiRecMode::StepInput {
                    return false;
                }
                let Some(p) = song.patterns.get_mut(pat) else {
                    return false;
                };
                if ch >= p.notes.len() {
                    return false;
                }
                let pat_len = p.len_ticks().max(1);
                let end_t = cur_tick % pat_len;
                let len = if end_t > held.start_tick {
                    (end_t - held.start_tick).max(1)
                } else if end_t == held.start_tick {
                    TICKS_PER_STEP
                } else {
                    // Note crossed the pattern loop boundary: clamp cleanly inside the bar.
                    (pat_len - held.start_tick).max(1)
                };
                p.notes[ch].push(Note::new_in(
                    held.start_tick,
                    len,
                    held.midi,
                    held.vel,
                    pat_len,
                ));
                true
            }
        }
    }
}

pub struct AudioInputHandle {
    _stream: Option<cpal::Stream>,
    pub device_name: String,
    pub sample_rate: f32,
}

impl AudioInputHandle {
    pub fn open(
        device_idx: usize,
        channel_offset: usize,
        stereo_pair: bool,
        mut prod: rtrb::Producer<f32>,
    ) -> Result<Self, String> {
        let host = cpal::default_host();
        let dev = host
            .input_devices()
            .map_err(|e| format!("{e}"))?
            .nth(device_idx)
            .ok_or_else(|| "audio input device not found".to_string())?;
        let device_name = dev
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| format!("Input {device_idx}"));
        let cfg = dev.default_input_config().map_err(|e| format!("{e}"))?;
        let sr = cfg.sample_rate() as f32;
        let channels = cfg.channels() as usize;
        let s_cfg: cpal::StreamConfig = cfg.into();
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => dev
                .build_input_stream(
                    s_cfg,
                    move |data: &[f32], _| {
                        push_input_samples(data, channels, channel_offset, stereo_pair, &mut prod);
                    },
                    |e| eprintln!("audio input error: {e}"),
                    None,
                )
                .map_err(|e| format!("{e}"))?,
            cpal::SampleFormat::I16 => dev
                .build_input_stream(
                    s_cfg,
                    move |data: &[i16], _| {
                        let tmp: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                        push_input_samples(&tmp, channels, channel_offset, stereo_pair, &mut prod);
                    },
                    |e| eprintln!("audio input error: {e}"),
                    None,
                )
                .map_err(|e| format!("{e}"))?,
            other => return Err(format!("unsupported input format: {other:?}")),
        };
        stream.play().map_err(|e| format!("{e}"))?;
        Ok(Self {
            _stream: Some(stream),
            device_name,
            sample_rate: sr,
        })
    }
}

fn push_input_samples(
    data: &[f32],
    channels: usize,
    offset: usize,
    stereo: bool,
    prod: &mut rtrb::Producer<f32>,
) {
    if channels == 0 {
        return;
    }
    let ch0 = offset.min(channels - 1);
    let ch1 = if stereo && ch0 + 1 < channels {
        ch0 + 1
    } else {
        ch0
    };
    for frame in data.chunks(channels) {
        if prod.slots() < 2 {
            break;
        }
        let l = frame.get(ch0).copied().unwrap_or(0.0);
        let r = frame.get(ch1).copied().unwrap_or(l);
        let _ = prod.push(l);
        let _ = prod.push(r);
    }
}

pub fn takes_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir);
    let dir = base.join("thunder").join("takes");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::song::new_empty;

    #[test]
    fn parse_midi_messages_and_learn_curves() {
        assert_eq!(
            parse_midi_bytes(&[0x92, 60, 127]),
            Some(MidiMsg::NoteOn {
                ch: 2,
                note: 60,
                vel: 1.0
            })
        );
        assert_eq!(
            parse_midi_bytes(&[0x92, 60, 0]),
            Some(MidiMsg::NoteOff { ch: 2, note: 60 })
        );
        assert_eq!(
            parse_midi_bytes(&[0xB0, 74, 64]),
            Some(MidiMsg::Cc {
                ch: 0,
                cc: 74,
                val: 64
            })
        );
        let Some(MidiMsg::PitchBend { ch, norm }) = parse_midi_bytes(&[0xE1, 0x00, 0x40]) else {
            panic!("expected pitch bend");
        };
        assert_eq!(ch, 1);
        assert!(norm.abs() < 1e-4);

        let mut song = new_empty();
        let mut learn = MidiLearnState {
            armed_target: Some(AutoTarget::ChannelVol(0)),
            pending_curve: MapCurve::Inverted,
            last_cc: None,
        };
        assert!(learn.process_cc(&mut song, 0, 7, 127));
        assert_eq!(song.midi_mappings.len(), 1);
        assert!(song.channels[0].volume < 1e-4);
        assert!(learn.process_cc(&mut song, 0, 7, 0));
        assert!((song.channels[0].volume - 1.5).abs() < 1e-4);
    }

    #[test]
    fn midi_recorder_overdub_replace_step_and_quantize() {
        let mut song = new_empty();
        song.auto_quantize_rec = true;
        let mut rec = MidiRecorder {
            armed: true,
            mode: MidiRecMode::Overdub,
            ..Default::default()
        };
        assert!(!rec.handle_msg(
            &mut song,
            MidiMsg::NoteOn {
                ch: 0,
                note: 48,
                vel: 0.8
            },
            MidiRecCtx {
                pat: 0,
                ch: 4,
                cur_tick: 5,
                playing: true,
                snap_grid: 4,
            }
        ));
        assert!(rec.handle_msg(
            &mut song,
            MidiMsg::NoteOff { ch: 0, note: 48 },
            MidiRecCtx {
                pat: 0,
                ch: 4,
                cur_tick: 13,
                playing: true,
                snap_grid: 4,
            }
        ));
        assert_eq!(song.patterns[0].notes[4].len(), 1);
        assert_eq!(song.patterns[0].notes[4][0].tick, 4);
        assert_eq!(song.patterns[0].notes[4][0].len, 9);

        rec.mode = MidiRecMode::Replace;
        rec.replaced_pass = false;
        assert!(!rec.handle_msg(
            &mut song,
            MidiMsg::NoteOn {
                ch: 0,
                note: 50,
                vel: 0.9
            },
            MidiRecCtx {
                pat: 0,
                ch: 4,
                cur_tick: 16,
                playing: true,
                snap_grid: 4,
            }
        ));
        assert!(rec.handle_msg(
            &mut song,
            MidiMsg::NoteOff { ch: 0, note: 50 },
            MidiRecCtx {
                pat: 0,
                ch: 4,
                cur_tick: 20,
                playing: true,
                snap_grid: 4,
            }
        ));
        assert_eq!(song.patterns[0].notes[4].len(), 1);
        assert_eq!(song.patterns[0].notes[4][0].midi, 50);

        rec.mode = MidiRecMode::StepInput;
        rec.step_cursor = 0;
        rec.step_len = 8;
        assert!(rec.handle_msg(
            &mut song,
            MidiMsg::NoteOn {
                ch: 0,
                note: 55,
                vel: 0.7
            },
            MidiRecCtx {
                pat: 0,
                ch: 4,
                cur_tick: 0,
                playing: false,
                snap_grid: 4,
            }
        ));
        assert_eq!(rec.step_cursor, 8);
        assert_eq!(song.patterns[0].notes[4].len(), 2);
    }

    #[test]
    fn device_probes_do_not_panic() {
        let _ = list_midi_inputs();
        let _ = list_midi_outputs();
        let _ = list_audio_inputs();
    }
}
