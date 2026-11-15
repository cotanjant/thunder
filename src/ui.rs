//! All egui panels: transport, channel rack, piano roll + automation,
//! playlist, mixer, sample browser.

use crate::engine::{AppState, load_sample_file, sample_into_song};
use crate::song::{
    AutoClipPoint, AutoPoint, AutoTarget, Channel, ChannelKind, ClipKind, KitPiece, Note, Song,
    TICKS_PER_STEP, TimeSig, WaveKind, eval_auto, eval_auto_clip, palette_color, tool_arpeggiate,
    tool_chord, tool_humanize_in, tool_quantize_in, tool_strum_in,
};
use eframe::egui;
use std::time::Instant;

// ---------------------------------------------------------------------------
// UI state (not part of the saved song)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScaleKind {
    Chromatic,
    Major,
    Minor,
    PentMaj,
    PentMin,
}

impl ScaleKind {
    fn name(&self) -> &'static str {
        match self {
            ScaleKind::Chromatic => "Chromatic",
            ScaleKind::Major => "Major",
            ScaleKind::Minor => "Minor",
            ScaleKind::PentMaj => "Pent Maj",
            ScaleKind::PentMin => "Pent Min",
        }
    }
    fn contains(&self, pc: u8, root: u8) -> bool {
        let rel = (12 + pc as i32 - (root % 12) as i32) % 12;
        match self {
            ScaleKind::Chromatic => true,
            ScaleKind::Major => matches!(rel, 0 | 2 | 4 | 5 | 7 | 9 | 11),
            ScaleKind::Minor => matches!(rel, 0 | 2 | 3 | 5 | 7 | 8 | 10),
            ScaleKind::PentMaj => matches!(rel, 0 | 2 | 4 | 7 | 9),
            ScaleKind::PentMin => matches!(rel, 0 | 3 | 5 | 7 | 10),
        }
    }
    /// Nearest pitch at or below `midi` that sits in the scale.
    fn snap_midi(&self, midi: u8, root: u8) -> u8 {
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
enum ChordMode {
    Off,
    Maj,
    Min,
}

/// FL-style piano roll tools. Draw places notes, Erase removes them with
/// left-click, Hear plays pitches without writing anything.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum PianoTool {
    #[default]
    Draw,
    Erase,
    Hear,
}

impl PianoTool {
    fn name(&self) -> &'static str {
        match self {
            PianoTool::Draw => "Draw",
            PianoTool::Erase => "Erase",
            PianoTool::Hear => "Hear",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AutoSel {
    Vol,
    Cut,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StepGraphMode {
    Off,
    Vel,
    Pan,
    Pitch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaylistPaint {
    Pattern,
    Audio,
    AutoChVol,
    AutoChCut,
    AutoInsertVol,
    AutoMasterVol,
}

impl PlaylistPaint {
    fn label(&self) -> &'static str {
        match self {
            PlaylistPaint::Pattern => "Pattern clip",
            PlaylistPaint::Audio => "Audio clip",
            PlaylistPaint::AutoChVol => "Auto: Ch Vol",
            PlaylistPaint::AutoChCut => "Auto: Ch Cut",
            PlaylistPaint::AutoInsertVol => "Auto: Ins Vol",
            PlaylistPaint::AutoMasterVol => "Auto: Master Vol",
        }
    }
}

#[derive(Clone, Copy)]
enum ClipDragKind {
    Move,
    Resize,
}

#[derive(Clone, Copy)]
enum DragKind {
    Move,
    Resize,
}

pub struct AppUi {
    pub sel_ch: usize,
    pub sel_pat: usize,
    pub sel_insert: usize,
    sel_track: usize,
    sel_clip: Option<u64>,
    clip_drag: Option<(u64, ClipDragKind, i32, u32)>,
    playlist_bars_view: u32,
    playlist_snap: u32,
    playlist_paint: PlaylistPaint,
    playlist_auto_drag: Option<(u64, usize)>,
    clip_sample: usize,
    show_ch_settings: bool,
    step_graph: StepGraphMode,
    route_target: usize,
    route_amount: f32,
    route_sidechain: bool,
    // FL-style window visibility (toolbar view toggles).
    pub show_browser: bool,
    pub show_rack: bool,
    pub show_piano: bool,
    pub show_playlist: bool,
    pub show_mixer: bool,
    tool: PianoTool,
    piano_bot: u8,
    snap: u32,
    scale: ScaleKind,
    root: u8,
    snap_scale: bool,
    chord: ChordMode,
    sel_note: Option<usize>,
    prop_note: Option<usize>,
    drag: Option<(usize, DragKind, i32, i32)>,
    press_cell: Option<(i32, i32)>,
    auto_sel: AutoSel,
    auto_drag: Option<usize>,
    save_path: String,
    wav_path: String,
    mid_path: String,
    take_path: String,
    sample_path: String,
    browser_dir: String,
    browser_cache: Vec<String>,
    note_clipboard: Vec<Note>,
    undo: Vec<Song>,
    redo: Vec<Song>,
    history_current: Option<Song>,
    history_pending: bool,
    pub status: String,
    taps: Vec<Instant>,
    show_tutorial: bool,
}

/// Marker file: first run shows the tutorial, afterwards it stays hidden.
fn tutorial_marker() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|p| p.join("thunder").join("tutorial_seen"))
}

fn tutorial_seen() -> bool {
    tutorial_marker().map(|p| p.exists()).unwrap_or(true)
}

fn set_tutorial_seen(seen: bool) {
    if let Some(p) = tutorial_marker() {
        if seen {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(p, "1").ok();
        } else {
            std::fs::remove_file(p).ok();
        }
    }
}

impl Default for AppUi {
    fn default() -> Self {
        Self {
            sel_ch: 4,
            sel_pat: 0,
            sel_insert: 1,
            sel_track: 0,
            sel_clip: None,
            clip_drag: None,
            playlist_bars_view: 8,
            playlist_snap: 16,
            playlist_paint: PlaylistPaint::Pattern,
            playlist_auto_drag: None,
            clip_sample: 0,
            show_ch_settings: false,
            step_graph: StepGraphMode::Off,
            route_target: 0,
            route_amount: 1.0,
            route_sidechain: false,
            show_browser: true,
            show_rack: true,
            show_piano: true,
            show_playlist: true,
            show_mixer: true,
            tool: PianoTool::Draw,
            piano_bot: 40,
            snap: 4,
            scale: ScaleKind::Major,
            root: 0,
            snap_scale: false,
            chord: ChordMode::Off,
            sel_note: None,
            prop_note: None,
            drag: None,
            press_cell: None,
            auto_sel: AutoSel::Vol,
            auto_drag: None,
            save_path: "thunder.json".into(),
            wav_path: "mixdown.wav".into(),
            mid_path: "song.mid".into(),
            take_path: "take.wav".into(),
            sample_path: String::new(),
            browser_dir: "samples".into(),
            browser_cache: Vec::new(),
            note_clipboard: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            history_current: None,
            history_pending: false,
            status: "press Play or open the Tutorial".into(),
            taps: Vec::new(),
            show_tutorial: !tutorial_seen(),
        }
    }
}

const HISTORY_LIMIT: usize = 100;

impl AppUi {
    pub fn record_song(&mut self, song: &Song, pointer_down: bool) {
        let Some(current) = &self.history_current else {
            self.history_current = Some(song.clone());
            return;
        };
        if current != song {
            if !pointer_down || !self.history_pending {
                if self.undo.len() == HISTORY_LIMIT {
                    self.undo.remove(0);
                }
                self.undo.push(current.clone());
                self.redo.clear();
                self.history_pending = pointer_down;
            }
            self.history_current = Some(song.clone());
        }
        if !pointer_down {
            self.history_pending = false;
        }
    }

    pub fn reset_history(&mut self, song: &Song) {
        self.undo.clear();
        self.redo.clear();
        self.history_current = Some(song.clone());
        self.history_pending = false;
    }

    fn undo_song(&mut self, song: &mut Song) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo.push(song.clone());
        *song = previous;
        self.history_current = Some(song.clone());
        self.history_pending = false;
        true
    }

    fn redo_song(&mut self, song: &mut Song) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(song.clone());
        *song = next;
        self.history_current = Some(song.clone());
        self.history_pending = false;
        true
    }

    fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

fn sync_runtime_after_history(s: &mut AppState) {
    let song = s.song.clone();
    s.rt.ensure_channels(&song);
    for i in 0..song.channels.len() {
        s.rt.rebuild_channel(&song, i);
    }
}

pub fn undo(ui: &mut AppUi, s: &mut AppState) -> bool {
    if !ui.undo_song(&mut s.song) {
        return false;
    }
    sync_runtime_after_history(s);
    ui.status = "undo".into();
    true
}

pub fn redo(ui: &mut AppUi, s: &mut AppState) -> bool {
    if !ui.redo_song(&mut s.song) {
        return false;
    }
    sync_runtime_after_history(s);
    ui.status = "redo".into();
    true
}

pub fn copy_notes(ui: &mut AppUi, s: &AppState) -> bool {
    let notes = s.song.patterns.get(ui.sel_pat).and_then(|pattern| {
        pattern
            .notes
            .get(ui.sel_ch.min(s.song.channels.len().saturating_sub(1)))
    });
    let Some(notes) = notes.filter(|notes| !notes.is_empty()) else {
        ui.status = "no notes to copy".into();
        return false;
    };
    ui.note_clipboard = notes.clone();
    ui.status = format!("copied {} notes", ui.note_clipboard.len());
    true
}

pub fn paste_notes(ui: &mut AppUi, s: &mut AppState) -> bool {
    if ui.note_clipboard.is_empty() {
        ui.status = "no copied notes".into();
        return false;
    }
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let Some(notes) = s
        .song
        .patterns
        .get_mut(ui.sel_pat)
        .and_then(|pattern| pattern.notes.get_mut(ch))
    else {
        return false;
    };
    notes.extend(ui.note_clipboard.iter().copied());
    notes.sort_by_key(|note| note.tick);
    ui.status = format!("pasted {} notes", ui.note_clipboard.len());
    true
}

pub fn link_selected_channel_to_insert(ui: &mut AppUi, s: &mut AppState) -> bool {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let ins = ui
        .sel_insert
        .min(s.song.mixer_tracks.len().saturating_sub(1));
    if let Some(c) = s.song.channels.get_mut(ch) {
        c.mixer_insert = ins;
        let ch_name = c.name.clone();
        let ins_name = s.song.mixer_tracks[ins].name.clone();
        ui.status = format!("linked {ch_name} -> {ins_name}");
        return true;
    }
    false
}

pub fn duplicate_pattern(ui: &mut AppUi, s: &mut AppState) -> bool {
    let source = ui.sel_pat.min(s.song.patterns.len().saturating_sub(1));
    let destination = (1..s.song.patterns.len())
        .map(|offset| (source + offset) % s.song.patterns.len())
        .find(|&idx| s.song.patterns[idx].is_empty());
    let dest = if let Some(d) = destination {
        s.song.patterns[d] = s.song.patterns[source].clone();
        d
    } else if let Some(d) = s.song.clone_pattern(source) {
        d
    } else {
        return false;
    };
    ui.sel_pat = dest;
    s.select_pattern(dest);
    ui.status = format!("duplicated P{} to P{}", source + 1, dest + 1);
    true
}

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

fn note_label(midi: u8) -> String {
    format!(
        "{}{}",
        NOTE_NAMES[(midi % 12) as usize],
        midi as i32 / 12 - 1
    )
}

fn color32(rgb: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn sample_files(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if let Some(ext) = p.extension().and_then(|x| x.to_str())
                && matches!(
                    ext.to_lowercase().as_str(),
                    "wav" | "mp3" | "ogg" | "flac" | "aiff" | "aif"
                )
            {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

fn home_music_dir() -> String {
    dirs::audio_dir()
        .map(|p| p.join("samples").to_string_lossy().into_owned())
        .unwrap_or_else(|| "samples".into())
}

const TIME_SIGS: [(u8, u8); 5] = [(4, 4), (3, 4), (6, 8), (5, 4), (7, 8)];

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

pub fn transport(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    if !s.song.patterns.is_empty() {
        ui.sel_pat = ui.sel_pat.min(s.song.patterns.len() - 1);
    }
    if !s.song.channels.is_empty() {
        ui.sel_ch = ui.sel_ch.min(s.song.channels.len() - 1);
    }
    if !s.song.mixer_tracks.is_empty() {
        ui.sel_insert = ui.sel_insert.min(s.song.mixer_tracks.len() - 1);
    }

    u.horizontal(|u| {
        // FL window toggles: browser, rack, piano, playlist, mixer.
        let mut v = ui.show_browser;
        if u.toggle_value(&mut v, "Files")
            .on_hover_text("Browser (F8)")
            .changed()
        {
            ui.show_browser = v;
        }
        let mut v = ui.show_rack;
        if u.toggle_value(&mut v, "Rack")
            .on_hover_text("Channel rack (F6)")
            .changed()
        {
            ui.show_rack = v;
        }
        let mut v = ui.show_piano;
        if u.toggle_value(&mut v, "Piano")
            .on_hover_text("Piano roll (F7)")
            .changed()
        {
            ui.show_piano = v;
        }
        let mut v = ui.show_playlist;
        if u.toggle_value(&mut v, "List")
            .on_hover_text("Playlist (F5)")
            .changed()
        {
            ui.show_playlist = v;
        }
        let mut v = ui.show_mixer;
        if u.toggle_value(&mut v, "Mix")
            .on_hover_text("Mixer (F9)")
            .changed()
        {
            ui.show_mixer = v;
        }
        u.separator();
        let playing = s.rt.playing;
        if u.button(if playing { "⏹ Stop" } else { "▶ Play" })
            .on_hover_text("Spacebar works too")
            .clicked()
        {
            s.set_playing(!playing);
        }
        if u.button("Tutorial")
            .on_hover_text("First-time walkthrough")
            .clicked()
        {
            ui.show_tutorial = !ui.show_tutorial;
        }
        if u.button("⏮ Reset").clicked() {
            s.reset_transport();
        }
        if u.add_enabled(ui.can_undo(), egui::Button::new("↶ Undo"))
            .on_hover_text("Undo (Ctrl/Cmd+Z)")
            .clicked()
        {
            undo(ui, s);
        }
        if u.add_enabled(ui.can_redo(), egui::Button::new("↷ Redo"))
            .on_hover_text("Redo (Ctrl/Cmd+Shift+Z or Ctrl/Cmd+Y)")
            .clicked()
        {
            redo(ui, s);
        }
        // Record the master bus. Starts playback too; pressing again keeps
        // the take and stops recording. A new take discards the old one.
        let mut rec = s.rt.recording;
        let rec_btn = u.toggle_value(&mut rec, "● Rec");
        if rec_btn
            .on_hover_text("Record what plays, save it below")
            .changed()
        {
            s.set_recording(rec, rec);
            ui.status = if rec {
                "recording…".into()
            } else {
                "take kept".into()
            };
        }
        let take_secs = s.take_duration_secs();
        u.label(format!(
            "{}take {}:{:02}{}",
            if s.rt.recording { "● " } else { "" },
            take_secs / 60,
            take_secs % 60,
            if s.rt.take_full { " (full)" } else { "" }
        ));
        // Song / pattern mode.
        let mut song_mode = s.rt.song_mode;
        if u.selectable_label(song_mode, "Song")
            .on_hover_text("Play the playlist arrangement")
            .clicked()
        {
            song_mode = true;
        }
        if u.selectable_label(!song_mode, "Pat")
            .on_hover_text("Loop one pattern")
            .clicked()
        {
            song_mode = false;
        }
        if song_mode != s.rt.song_mode {
            s.set_song_mode(song_mode);
        }
        let mut pat = ui.sel_pat;
        let sel_pat_name = s
            .song
            .patterns
            .get(pat)
            .map(|p| format!("P{}: {}", pat + 1, p.name))
            .unwrap_or_else(|| format!("P{}", pat + 1));
        egui::ComboBox::from_id_salt("top_pat_pick")
            .selected_text(sel_pat_name)
            .show_ui(u, |u| {
                for (p, pdata) in s.song.patterns.iter().enumerate() {
                    u.selectable_value(&mut pat, p, format!("P{}: {}", p + 1, pdata.name));
                }
            });
        if pat != ui.sel_pat {
            ui.sel_pat = pat;
            if !song_mode {
                s.set_pattern(pat);
            } else {
                s.select_pattern(pat);
            }
        }
        if u.button("+ Pat").on_hover_text("Add pattern").clicked() {
            let np = s.song.add_pattern(None);
            ui.sel_pat = np;
            s.select_pattern(np);
            ui.status = format!("added P{}", np + 1);
        }
        if u.button("Dup Pat")
            .on_hover_text("Clone active pattern")
            .clicked()
            && let Some(np) = s.song.clone_pattern(ui.sel_pat)
        {
            ui.sel_pat = np;
            s.select_pattern(np);
            ui.status = format!("cloned to P{}", np + 1);
        }
        if u.add_enabled(s.song.patterns.len() > 1, egui::Button::new("- Pat"))
            .on_hover_text("Delete active pattern")
            .clicked()
            && s.song.delete_pattern(ui.sel_pat)
        {
            ui.sel_pat = ui.sel_pat.min(s.song.patterns.len() - 1);
            s.select_pattern(ui.sel_pat);
            ui.status = "pattern deleted".into();
        }
        let mut bpm = s.song.bpm;
        u.add(egui::Slider::new(&mut bpm, 60.0..=200.0).text("BPM"));
        if (bpm - s.song.bpm).abs() > f32::EPSILON {
            s.song.bpm = bpm;
        }
        if u.button("Tap").clicked() {
            let now = Instant::now();
            ui.taps.push(now);
            ui.taps
                .retain(|t| now.duration_since(*t).as_secs_f32() < 2.5);
            if ui.taps.len() >= 3 {
                let diffs: Vec<f32> = ui
                    .taps
                    .windows(2)
                    .map(|w| w[1].duration_since(w[0]).as_secs_f32())
                    .collect();
                let avg = diffs.iter().sum::<f32>() / diffs.len() as f32;
                if avg > 0.15 && avg < 2.0 {
                    s.song.bpm = (60.0 / avg).clamp(60.0, 200.0);
                }
            }
        }
        let mut swing = s.song.swing;
        u.add(egui::Slider::new(&mut swing, 0.0..=0.6).text("Swing"));
        if (swing - s.song.swing).abs() > f32::EPSILON {
            s.song.swing = swing;
        }
        let mut metro = s.song.metronome;
        u.toggle_value(&mut metro, "Click");
        s.song.metronome = metro;
        let mut lp = s.song.loop_song;
        u.toggle_value(&mut lp, "Loop");
        s.song.loop_song = lp;
        let mut master = s.song.master_vol;
        u.add(egui::Slider::new(&mut master, 0.0..=1.5).text("Master"));
        s.song.master_vol = master;
        // Position readout.
        let tick = s.rt.tick_pos.max(0.0) as u64;
        let bar_ticks = s.song.time_sig.ticks_per_bar().max(1) as u64;
        let beat_ticks = s.song.time_sig.ticks_per_beat().max(1) as u64;
        u.label(format!(
            "{:02}:{:01}:{:02}",
            tick / bar_ticks + 1,
            (tick % bar_ticks) / beat_ticks + 1,
            tick % beat_ticks
        ));
        u.add(
            egui::ProgressBar::new(s.rt.peak.clamp(0.0, 1.0))
                .text(format!("{:.0}%", s.rt.peak * 100.0)),
        );
        if s.rt.xruns > 0 {
            u.colored_label(
                egui::Color32::from_rgb(230, 160, 60),
                format!("xruns:{}", s.rt.xruns),
            );
        }
    });
    u.horizontal(|u| {
        // Active pattern & project time signature bar.
        if let Some(pdata) = s.song.patterns.get_mut(ui.sel_pat) {
            u.label("Pat name:");
            u.add(egui::TextEdit::singleline(&mut pdata.name).desired_width(110.0));
            let mut bars = pdata.bars;
            u.add(
                egui::DragValue::new(&mut bars)
                    .range(1..=16)
                    .prefix("Bars: "),
            );
            pdata.bars = bars.max(1);
            let mut ts = pdata.time_sig;
            egui::ComboBox::from_id_salt("pat_timesig")
                .selected_text(format!("Sig {}", ts.label()))
                .show_ui(u, |u| {
                    for (num, den) in TIME_SIGS {
                        let cand = TimeSig { num, den };
                        u.selectable_value(&mut ts, cand, cand.label());
                    }
                });
            pdata.time_sig = ts;
        }
        let mut proj_ts = s.song.time_sig;
        egui::ComboBox::from_id_salt("proj_timesig")
            .selected_text(format!("Song {}", proj_ts.label()))
            .show_ui(u, |u| {
                for (num, den) in TIME_SIGS {
                    let cand = TimeSig { num, den };
                    u.selectable_value(&mut proj_ts, cand, cand.label());
                }
            });
        s.song.time_sig = proj_ts;
        u.separator();
        u.label("Project:");
        u.add(egui::TextEdit::singleline(&mut ui.save_path).desired_width(140.0));
        if u.button("Save").clicked() {
            match s.song.save(&ui.save_path.clone()) {
                Ok(()) => ui.status = format!("saved {}", ui.save_path),
                Err(e) => ui.status = format!("save failed: {e}"),
            }
        }
        if u.button("Load").clicked() {
            match Song::load(&ui.save_path.clone()) {
                Ok(mut song) => {
                    let mut missing = 0;
                    for smp in song.samples.iter_mut() {
                        match load_sample_file(&smp.path) {
                            Ok((data, sr)) => {
                                smp.data = data.into();
                                smp.sr = sr;
                            }
                            Err(_) => missing += 1,
                        }
                    }
                    ui.sel_ch = ui.sel_ch.min(song.channels.len().saturating_sub(1));
                    ui.sel_pat = ui.sel_pat.min(song.patterns.len().saturating_sub(1));
                    ui.sel_insert = ui.sel_insert.min(song.mixer_tracks.len().saturating_sub(1));
                    s.apply_loaded_song(song);
                    s.set_pattern(ui.sel_pat);
                    ui.reset_history(&s.song);
                    ui.status = if missing == 0 {
                        format!("loaded {}", ui.save_path)
                    } else {
                        format!("loaded {} ({missing} samples missing)", ui.save_path)
                    };
                }
                Err(e) => ui.status = format!("load failed: {e}"),
            }
        }
        u.label("WAV:");
        u.add(egui::TextEdit::singleline(&mut ui.wav_path).desired_width(100.0));
        if u.button("Export").clicked() {
            match crate::export::export_wav(&s.song, &ui.wav_path.clone()) {
                Ok(m) => ui.status = m,
                Err(e) => ui.status = format!("wav export failed: {e}"),
            }
        }
        u.label("MIDI:");
        u.add(egui::TextEdit::singleline(&mut ui.mid_path).desired_width(90.0));
        if u.button("Export").clicked() {
            match crate::export::export_midi(&s.song, ui.sel_pat, &ui.mid_path.clone()) {
                Ok(m) => ui.status = m,
                Err(e) => ui.status = format!("midi export failed: {e}"),
            }
        }
        u.label("Take:");
        u.add(egui::TextEdit::singleline(&mut ui.take_path).desired_width(90.0));
        if u.button("Save").clicked() {
            if !s.has_take() {
                ui.status = "nothing recorded yet".into();
            } else {
                match s.save_take(&ui.take_path.clone()) {
                    Ok(n) => ui.status = format!("saved take ({n} samples)"),
                    Err(e) => ui.status = format!("take save failed: {e}"),
                }
            }
        }
    });
}

pub fn channel_rack(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.horizontal(|u| {
        u.heading("Channel rack");
        if u.button("+ Synth").clicked() {
            let idx = s.song.channels.len();
            let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
            let c = Channel {
                name: format!("Synth {}", idx + 1),
                color: palette_color(idx),
                mixer_insert: ins,
                ..Default::default()
            };
            ui.sel_ch = s.song.add_channel(c);
            s.rebuild_channel(ui.sel_ch);
            ui.status = format!("added Synth {}", idx + 1);
        }
        if u.button("+ Kit").clicked() {
            let idx = s.song.channels.len();
            let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
            let c = Channel {
                name: format!("Kit {}", idx + 1),
                kind: ChannelKind::Kit {
                    piece: KitPiece::Kick,
                },
                color: palette_color(idx),
                cutoff: 12000.0,
                mixer_insert: ins,
                ..Default::default()
            };
            ui.sel_ch = s.song.add_channel(c);
            ui.status = format!("added Kit {}", idx + 1);
        }
        if u.button("+ Sampler").clicked() {
            let idx = s.song.channels.len();
            let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
            let c = Channel {
                name: format!("Sampler {}", idx + 1),
                kind: ChannelKind::Sampler { sample: None },
                color: palette_color(idx),
                cutoff: 12000.0,
                mixer_insert: ins,
                ..Default::default()
            };
            ui.sel_ch = s.song.add_channel(c);
            ui.status = format!("added Sampler {}", idx + 1);
        }
        u.toggle_value(&mut ui.show_ch_settings, "Settings");
        u.label("Graph:");
        u.selectable_value(&mut ui.step_graph, StepGraphMode::Off, "Off");
        u.selectable_value(&mut ui.step_graph, StepGraphMode::Vel, "Vel");
        u.selectable_value(&mut ui.step_graph, StepGraphMode::Pan, "Pan");
        u.selectable_value(&mut ui.step_graph, StepGraphMode::Pitch, "Pitch");
    });

    let pat = ui.sel_pat.min(s.song.patterns.len().saturating_sub(1));
    let pat_steps = s.song.patterns.get(pat).map(|p| p.steps()).unwrap_or(16);
    let beat_steps = s
        .song
        .patterns
        .get(pat)
        .map(|p| (p.time_sig.ticks_per_beat() / TICKS_PER_STEP).max(1) as usize)
        .unwrap_or(4);

    let mut action: Option<(&'static str, usize)> = None;
    let n = s.song.channels.len();
    for i in 0..n {
        let ch_color = color32(s.song.channels[i].color);
        u.horizontal(|u| {
            let (swatch, swatch_resp) =
                u.allocate_exact_size(egui::vec2(8.0, 22.0), egui::Sense::click());
            u.painter_at(swatch).rect_filled(swatch, 2.0, ch_color);
            if swatch_resp.clicked() {
                s.song.channels[i].color = palette_color(i + 1);
            }
            if u.selectable_label(ui.sel_ch == i, s.song.channels[i].name.clone())
                .clicked()
            {
                ui.sel_ch = i;
            }
            let mut m = s.song.channels[i].mute;
            if u.toggle_value(&mut m, "M").changed() {
                s.song.channels[i].mute = m;
            }
            let mut so = s.song.channels[i].solo;
            if u.toggle_value(&mut so, "S").changed() {
                s.song.channels[i].solo = so;
            }
            // Mixer Insert target.
            let mut ins = s.song.channels[i].mixer_insert;
            let ins_txt = if ins == 0 {
                "Mst".to_string()
            } else {
                format!("I{ins}")
            };
            egui::ComboBox::from_id_salt(format!("ch_ins_{i}"))
                .width(48.0)
                .selected_text(ins_txt)
                .show_ui(u, |u| {
                    for (mi, mt) in s.song.mixer_tracks.iter().enumerate() {
                        u.selectable_value(&mut ins, mi, mt.name.clone());
                    }
                });
            s.song.channels[i].mixer_insert = ins;

            // Kind picker.
            match s.song.channels[i].kind.clone() {
                ChannelKind::Synth { wave } => {
                    let mut w = wave;
                    egui::ComboBox::from_id_salt(format!("wave{i}"))
                        .width(62.0)
                        .selected_text(w.name())
                        .show_ui(u, |u| {
                            for cand in WaveKind::all() {
                                u.selectable_value(&mut w, cand, cand.name());
                            }
                        });
                    if w != wave {
                        s.song.channels[i].kind = ChannelKind::Synth { wave: w };
                        s.rebuild_channel(i);
                    }
                }
                ChannelKind::Kit { piece } => {
                    let mut p = piece;
                    egui::ComboBox::from_id_salt(format!("kit{i}"))
                        .width(62.0)
                        .selected_text(p.name())
                        .show_ui(u, |u| {
                            for cand in KitPiece::all() {
                                u.selectable_value(&mut p, cand, cand.name());
                            }
                        });
                    if p != piece {
                        s.song.channels[i].kind = ChannelKind::Kit { piece: p };
                    }
                }
                ChannelKind::Sampler { sample } => {
                    let txt = sample
                        .and_then(|idx| s.song.samples.get(idx))
                        .map(|x| x.name.clone())
                        .unwrap_or_else(|| "(empty)".into());
                    u.label(txt);
                }
            }
            if u.button("♪").on_hover_text("Preview").clicked() {
                preview_channel(s, i);
            }
            if u.small_button("Dup")
                .on_hover_text("Duplicate channel")
                .clicked()
            {
                action = Some(("dup", i));
            }
            if u.add_enabled(i > 0, egui::Button::new("^").small())
                .clicked()
            {
                action = Some(("up", i));
            }
            if u.add_enabled(i + 1 < n, egui::Button::new("v").small())
                .clicked()
            {
                action = Some(("down", i));
            }
            if u.add_enabled(n > 1, egui::Button::new("×").small())
                .on_hover_text("Delete channel")
                .clicked()
            {
                action = Some(("del", i));
            }

            // Step buttons across variable pattern length.
            let playing_step = if s.rt.playing && pat == s.rt.cur_pat {
                Some((s.rt.cur_tick / TICKS_PER_STEP) as usize)
            } else {
                None
            };
            egui::ScrollArea::horizontal()
                .id_salt(format!("steps_scroll_{i}"))
                .max_height(28.0)
                .show(u, |u| {
                    u.horizontal(|u| {
                        for step in 0..pat_steps {
                            let active = s.song.step_active(pat, i, step);
                            let mut btn = egui::Button::new(format!("{}", step + 1))
                                .min_size(egui::vec2(24.0, 22.0));
                            if active && playing_step == Some(step) {
                                btn = btn.fill(egui::Color32::LIGHT_GREEN);
                            } else if active {
                                btn = btn.fill(egui::Color32::DARK_GREEN);
                            } else if playing_step == Some(step) {
                                btn = btn.fill(egui::Color32::from_rgb(90, 70, 20));
                            } else if (step / beat_steps).is_multiple_of(2) {
                                btn = btn.fill(egui::Color32::from_rgb(55, 55, 60));
                            }
                            if u.add(btn).clicked() {
                                s.song.toggle_step(pat, i, step);
                            }
                        }
                    });
                });
        });
    }

    if let Some((op, i)) = action {
        match op {
            "dup" => {
                if let Some(ni) = s.song.duplicate_channel(i) {
                    ui.sel_ch = ni;
                    s.rebuild_channel(ni);
                    ui.status = "duplicated channel".into();
                }
            }
            "up" if i > 0 => {
                if s.song.move_channel(i, i - 1) {
                    if ui.sel_ch == i {
                        ui.sel_ch = i - 1;
                    } else if ui.sel_ch == i - 1 {
                        ui.sel_ch = i;
                    }
                    sync_runtime_after_history(s);
                }
            }
            "down" => {
                if s.song.move_channel(i, i + 1) {
                    if ui.sel_ch == i {
                        ui.sel_ch = i + 1;
                    } else if ui.sel_ch == i + 1 {
                        ui.sel_ch = i;
                    }
                    sync_runtime_after_history(s);
                }
            }
            "del" if s.song.delete_channel(i) => {
                ui.sel_ch = ui.sel_ch.min(s.song.channels.len() - 1);
                sync_runtime_after_history(s);
                ui.status = "deleted channel".into();
            }
            _ => {}
        }
    }

    if ui.show_ch_settings {
        channel_settings_panel(ui, s, u);
    }
    if ui.step_graph != StepGraphMode::Off {
        step_graph_editor(ui, s, pat, pat_steps, u);
    }
}

fn channel_settings_panel(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let mut need_rebuild = false;
    let mut need_cutoff = None;
    let mix_names: Vec<String> = s.song.mixer_tracks.iter().map(|m| m.name.clone()).collect();
    if let Some(c) = s.song.channels.get_mut(ch) {
        u.group(|u| {
            u.horizontal(|u| {
                u.label("Channel settings:");
                u.add(egui::TextEdit::singleline(&mut c.name).desired_width(110.0));
                let cur_name = mix_names
                    .get(c.mixer_insert)
                    .cloned()
                    .unwrap_or_else(|| "Master".into());
                egui::ComboBox::from_id_salt("ch_set_ins")
                    .selected_text(cur_name)
                    .show_ui(u, |u| {
                        for (mi, name) in mix_names.iter().enumerate() {
                            u.selectable_value(&mut c.mixer_insert, mi, name.clone());
                        }
                    });
                u.add(egui::Slider::new(&mut c.midi_base, 21..=108).text("Root"));
                let mut poly = c.polyphony;
                u.add(
                    egui::DragValue::new(&mut poly)
                        .range(0..=8)
                        .prefix("Poly: "),
                );
                c.polyphony = poly;
                u.add(egui::Slider::new(&mut c.portamento, 0.0..=0.5).text("Porta"));
            });
            u.horizontal(|u| {
                let old_spec = c.voice_spec();
                u.add(
                    egui::Slider::new(&mut c.cutoff, 80.0..=16000.0)
                        .logarithmic(true)
                        .text("Cut"),
                );
                need_cutoff = Some(c.cutoff);
                u.add(egui::Slider::new(&mut c.q, 0.2..=6.0).text("Q"));
                u.add(egui::Slider::new(&mut c.attack, 0.001..=1.0).text("A"));
                u.add(egui::Slider::new(&mut c.decay, 0.005..=2.0).text("D"));
                u.add(egui::Slider::new(&mut c.sustain, 0.0..=1.0).text("S"));
                u.add(egui::Slider::new(&mut c.release, 0.005..=2.0).text("R"));
                if c.voice_spec() != old_spec {
                    need_rebuild = true;
                }
            });
            u.horizontal(|u| {
                u.label("Cut group:");
                let mut cg = c.cut_group.unwrap_or(0);
                u.add(egui::DragValue::new(&mut cg).range(0..=8));
                c.cut_group = (cg > 0).then_some(cg);
                u.label("Cut by:");
                let mut cb = c.cut_by.unwrap_or(0);
                u.add(egui::DragValue::new(&mut cb).range(0..=8));
                c.cut_by = (cb > 0).then_some(cb);
                u.add(egui::Slider::new(&mut c.volume, 0.0..=1.5).text("Vol"));
                u.add(egui::Slider::new(&mut c.pan, -1.0..=1.0).text("Pan"));
                u.add(egui::Slider::new(&mut c.delay_send, 0.0..=1.0).text("Dly"));
                u.add(egui::Slider::new(&mut c.reverb_send, 0.0..=1.0).text("Verb"));
            });
        });
    }
    if need_rebuild {
        s.rebuild_channel(ch);
    } else if let Some(cut) = need_cutoff {
        s.set_channel_cutoff(ch, cut);
    }
}

fn step_graph_editor(ui: &mut AppUi, s: &mut AppState, pat: usize, steps: usize, u: &mut egui::Ui) {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let label = match ui.step_graph {
        StepGraphMode::Vel => "Step graph: Velocity",
        StepGraphMode::Pan => "Step graph: Pan (-1..+1)",
        StepGraphMode::Pitch => "Step graph: Fine pitch (-12..+12 st)",
        StepGraphMode::Off => return,
    };
    u.label(label);
    let w = (steps as f32 * 26.0).clamp(240.0, u.available_width().max(240.0));
    let h = 58.0;
    let (rect, resp) = u.allocate_exact_size(egui::vec2(w, h), egui::Sense::click_and_drag());
    let painter = u.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(16));
    let col_w = rect.width() / steps.max(1) as f32;
    let notes = s
        .song
        .patterns
        .get(pat)
        .and_then(|p| p.notes.get(ch))
        .cloned()
        .unwrap_or_default();

    for step in 0..steps {
        let t0 = step as u32 * TICKS_PER_STEP;
        let t1 = t0 + TICKS_PER_STEP;
        let x0 = rect.min.x + step as f32 * col_w;
        if let Some(n) = notes.iter().find(|n| n.tick >= t0 && n.tick < t1) {
            let (norm, col) = match ui.step_graph {
                StepGraphMode::Vel => {
                    (n.vel.clamp(0.0, 1.0), egui::Color32::from_rgb(90, 180, 110))
                }
                StepGraphMode::Pan => (
                    (n.pan.clamp(-1.0, 1.0) + 1.0) * 0.5,
                    egui::Color32::from_rgb(100, 165, 220),
                ),
                StepGraphMode::Pitch => (
                    (n.fine_pitch.clamp(-12.0, 12.0) + 12.0) / 24.0,
                    egui::Color32::from_rgb(220, 160, 85),
                ),
                StepGraphMode::Off => (0.0, egui::Color32::GRAY),
            };
            let bh = norm * (h - 6.0);
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x0 + 2.0, rect.max.y - 3.0 - bh),
                    egui::vec2((col_w - 4.0).max(2.0), bh),
                ),
                1.0,
                col,
            );
        }
    }
    if (resp.dragged() || resp.clicked())
        && let Some(pos) = resp.hover_pos()
    {
        let step = (((pos.x - rect.min.x) / col_w).floor() as usize).min(steps.saturating_sub(1));
        let norm = (1.0 - (pos.y - rect.min.y) / h).clamp(0.0, 1.0);
        let t0 = step as u32 * TICKS_PER_STEP;
        let t1 = t0 + TICKS_PER_STEP;
        if let Some(list) = s
            .song
            .patterns
            .get_mut(pat)
            .and_then(|p| p.notes.get_mut(ch))
        {
            for n in list.iter_mut().filter(|n| n.tick >= t0 && n.tick < t1) {
                match ui.step_graph {
                    StepGraphMode::Vel => n.vel = norm.clamp(0.05, 1.0),
                    StepGraphMode::Pan => n.pan = norm * 2.0 - 1.0,
                    StepGraphMode::Pitch => n.fine_pitch = (norm * 24.0 - 12.0).round(),
                    StepGraphMode::Off => {}
                }
            }
        }
    }
}

fn preview_channel(s: &mut AppState, i: usize) {
    match s.song.channels[i].kind.clone() {
        ChannelKind::Synth { .. } => s.live_note_ch(i, s.song.channels[i].midi_base),
        ChannelKind::Kit { piece } => s.live_kit(piece),
        ChannelKind::Sampler { sample } => {
            if let Some(idx) = sample {
                s.live_sample(idx);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Piano roll
// ---------------------------------------------------------------------------

const PIANO_ROWS: u8 = 37;
const KEY_W: f32 = 52.0;

fn piano_midi_range(bot: u8) -> (u8, u8) {
    let top = (bot + PIANO_ROWS - 1).min(108);
    (bot.max(21), top)
}

/// Snap a pitch into the scale when snap-to-scale is on, else pass through.
fn scale_checked(ui: &AppUi, midi: u8) -> u8 {
    if ui.snap_scale {
        ui.scale.snap_midi(midi, ui.root)
    } else {
        midi
    }
}

/// Add a single note or a chord stamp, then report it in the status line.
fn place_note(ui: &mut AppUi, s: &mut AppState, ch: usize, pat: usize, tick: u32, midi: u8) {
    match ui.chord {
        ChordMode::Off => {
            if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
                notes.push(Note::new(tick, ui.snap.max(1), midi, 0.85));
                notes.sort_by_key(|n| n.tick);
                ui.sel_note = notes.iter().rposition(|n| n.tick == tick && n.midi == midi);
                ui.status = format!("note {} at tick {tick}", note_label(midi));
            }
        }
        ChordMode::Maj | ChordMode::Min => {
            let minor = ui.chord == ChordMode::Min;
            if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
                notes.extend(tool_chord(tick, midi, minor));
                notes.sort_by_key(|n| n.tick);
                ui.status = format!(
                    "{} chord at tick {tick}",
                    if minor { "minor" } else { "major" }
                );
            }
        }
    }
}

pub fn piano_roll(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    piano_toolbar(ui, s, u);
    // Target switch above may have changed the channel: re-resolve.
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let pat = ui.sel_pat;
    piano_options(ui, u);
    piano_canvas(ui, s, ch, pat, u);
    velocity_lane(s, ch, pat, u);
    automation_lane(ui, s, ch, pat, u);
    note_dialog(ui, s, ch, pat, u);
}

fn piano_toolbar(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let pat = ui.sel_pat;
    u.horizontal(|u| {
        u.heading(format!(
            "Piano roll : {} / P{}",
            s.song.channels[ch].name,
            pat + 1
        ));
        // FL-style tool modes: draw writes, erase deletes, hear only plays.
        for tool in [PianoTool::Draw, PianoTool::Erase, PianoTool::Hear] {
            if u.selectable_label(ui.tool == tool, tool.name())
                .on_hover_text(match tool {
                    PianoTool::Draw => "Draw: click adds a note, drag moves it (P)",
                    PianoTool::Erase => "Erase: left-click deletes notes (D)",
                    PianoTool::Hear => "Hear: click plays pitches, writes nothing (Y)",
                })
                .clicked()
            {
                ui.tool = tool;
            }
        }
        // Target channel: switch instrument without leaving the piano roll.
        let mut target = ch;
        egui::ComboBox::from_id_salt("target_ch")
            .selected_text(s.song.channels[ch].name.clone())
            .show_ui(u, |u| {
                for (i, c) in s.song.channels.iter().enumerate() {
                    let tick = s.song.patterns[pat]
                        .notes
                        .get(i)
                        .map(|ns| !ns.is_empty())
                        .unwrap_or(false);
                    u.selectable_value(
                        &mut target,
                        i,
                        format!("{}{}", if tick { "● " } else { "" }, c.name),
                    );
                }
            });
        if target != ch {
            ui.sel_ch = target;
            ui.sel_note = None;
            ui.prop_note = None;
            return;
        }
        if u.button("Copy")
            .on_hover_text("Copy this channel's notes (Ctrl/Cmd+C)")
            .clicked()
        {
            copy_notes(ui, s);
        }
        if u.add_enabled(!ui.note_clipboard.is_empty(), egui::Button::new("Paste"))
            .on_hover_text("Paste copied notes into this channel (Ctrl/Cmd+V)")
            .clicked()
        {
            paste_notes(ui, s);
        }
        if u.button("Clone P")
            .on_hover_text("Copy this pattern to the next empty pattern slot")
            .clicked()
        {
            duplicate_pattern(ui, s);
        }
        if u.button("Quant")
            .on_hover_text("Snap starts to grid")
            .clicked()
        {
            let max_ticks = s.song.patterns[pat].len_ticks();
            if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
                tool_quantize_in(notes, ui.snap, max_ticks);
                ui.status = "quantized".into();
            }
        }
        if u.button("Human")
            .on_hover_text("Timing + velocity feel")
            .clicked()
        {
            let max_ticks = s.song.patterns[pat].len_ticks();
            if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(42);
                tool_humanize_in(notes, seed, max_ticks);
            }
            ui.status = "humanized".into();
        }
        if u.button("Strum").on_hover_text("Spread chords").clicked() {
            let max_ticks = s.song.patterns[pat].len_ticks();
            if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
                tool_strum_in(notes, 2, max_ticks);
                ui.status = "strummed".into();
            }
        }
        if u.button("Arp")
            .on_hover_text("16th arpeggio from pitches")
            .clicked()
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
        {
            tool_arpeggiate(notes);
            ui.status = "arpeggiated".into();
        }
        if u.button("Chord: off").clicked() {
            ui.chord = match ui.chord {
                ChordMode::Off => ChordMode::Maj,
                ChordMode::Maj => ChordMode::Min,
                ChordMode::Min => ChordMode::Off,
            };
        }
        let chord_txt = match ui.chord {
            ChordMode::Off => "Chord: off",
            ChordMode::Maj => "Chord: Maj",
            ChordMode::Min => "Chord: min",
        };
        u.label(chord_txt);
        if u.button("Clear").clicked()
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
        {
            notes.clear();
        }
        if u.button("Oct−").clicked()
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
        {
            for n in notes.iter_mut() {
                n.midi = n.midi.saturating_sub(12).max(21);
            }
        }
        if u.button("Oct+").clicked()
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
        {
            for n in notes.iter_mut() {
                n.midi = (n.midi + 12).min(108);
            }
        }
    });
}

fn piano_options(ui: &mut AppUi, u: &mut egui::Ui) {
    u.horizontal(|u| {
        u.label("Snap:");
        egui::ComboBox::from_id_salt("snap")
            .selected_text(format!("1/{}", 64 / ui.snap.max(1)))
            .show_ui(u, |u| {
                for sn in [1u32, 2, 4] {
                    u.selectable_value(&mut ui.snap, sn, format!("1/{}", 64 / sn));
                }
            });
        u.label("Scale:");
        let mut sc = ui.scale;
        egui::ComboBox::from_id_salt("scale")
            .selected_text(sc.name())
            .show_ui(u, |u| {
                for cand in [
                    ScaleKind::Chromatic,
                    ScaleKind::Major,
                    ScaleKind::Minor,
                    ScaleKind::PentMaj,
                    ScaleKind::PentMin,
                ] {
                    u.selectable_value(&mut sc, cand, cand.name());
                }
            });
        ui.scale = sc;
        u.label("Root:");
        let mut root = ui.root;
        egui::ComboBox::from_id_salt("root")
            .selected_text(NOTE_NAMES[(root % 12) as usize])
            .show_ui(u, |u| {
                for r in 0u8..12 {
                    u.selectable_value(&mut root, r, NOTE_NAMES[r as usize]);
                }
            });
        ui.root = root;
        let mut snap_scale = ui.snap_scale;
        if u.toggle_value(&mut snap_scale, "ScaleSnap")
            .on_hover_text("Force added and moved notes into the scale")
            .changed()
        {
            ui.snap_scale = snap_scale;
        }
        let mut bot = ui.piano_bot;
        u.add(egui::Slider::new(&mut bot, 21..=72).text("Octave"));
        ui.piano_bot = bot;
    });
}

fn piano_canvas(
    ui: &mut AppUi,
    s: &mut AppState,
    ch: usize,
    pat: usize,
    u: &mut egui::Ui,
) -> egui::Rect {
    let (midi_lo, midi_hi) = piano_midi_range(ui.piano_bot);
    let rows = (midi_hi - midi_lo + 1) as usize;
    let pdata = &s.song.patterns[pat];
    let pat_ticks = pdata.len_ticks().max(1);
    let bar_ticks = pdata.time_sig.ticks_per_bar().max(1);
    let beat_ticks = pdata.time_sig.ticks_per_beat().max(1);
    let max_tick = (pat_ticks - 1) as i32;
    let avail_w = u.available_width().max(300.0);
    let cell_w = (avail_w - KEY_W) / pat_ticks as f32;
    let cell_h = 9.0;
    let h = rows as f32 * cell_h;
    let (rect, resp) = u.allocate_exact_size(egui::vec2(avail_w, h), egui::Sense::click_and_drag());
    let painter = u.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(16));

    let tick_at = |x: f32| ((x - rect.min.x - KEY_W) / cell_w).floor() as i32;
    let midi_at = |y: f32| midi_hi as i32 - ((y - rect.min.y) / cell_h).floor() as i32;
    let x_of = |tick: u32| rect.min.x + KEY_W + tick as f32 * cell_w;
    let y_of = |midi: u8| rect.min.y + (midi_hi - midi) as f32 * cell_h;

    // Scale shading + key strip.
    for r in 0..rows {
        let midi = midi_hi - r as u8;
        let y = rect.min.y + r as f32 * cell_h;
        let in_scale = ui.scale.contains(midi % 12, ui.root);
        let row_col = if !in_scale {
            egui::Color32::from_gray(22)
        } else if midi.is_multiple_of(12) {
            egui::Color32::from_rgb(34, 34, 40)
        } else {
            egui::Color32::from_gray(28)
        };
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(rect.min.x + KEY_W, y),
                egui::vec2(rect.width() - KEY_W, cell_h),
            ),
            0.0,
            row_col,
        );
        let black = matches!(midi % 12, 1 | 3 | 6 | 8 | 10);
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(rect.min.x, y), egui::vec2(KEY_W, cell_h)),
            0.0,
            if black {
                egui::Color32::BLACK
            } else {
                egui::Color32::from_gray(200)
            },
        );
        if midi.is_multiple_of(12) {
            painter.text(
                egui::pos2(rect.min.x + 4.0, y + cell_h * 0.5),
                egui::Align2::LEFT_CENTER,
                note_label(midi),
                egui::FontId::proportional(9.0),
                if black {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::BLACK
                },
            );
        }
    }
    // Grid lines.
    for t in 0u32..=pat_ticks {
        let x = rect.min.x + KEY_W + t as f32 * cell_w;
        let strong = t.is_multiple_of(bar_ticks) || t == 0;
        let mid = t.is_multiple_of(beat_ticks);
        let step_line = t.is_multiple_of(TICKS_PER_STEP);
        if !strong && !mid && !step_line && cell_w < 6.0 {
            continue;
        }
        painter.line_segment(
            [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
            egui::Stroke::new(
                1.0_f32,
                if strong {
                    egui::Color32::from_gray(90)
                } else if mid {
                    egui::Color32::from_gray(55)
                } else {
                    egui::Color32::from_gray(38)
                },
            ),
        );
    }
    // Ghost notes from other channels.
    if let Some(pdata) = s.song.patterns.get(pat) {
        for (ci, notes) in pdata.notes.iter().enumerate() {
            if ci == ch {
                continue;
            }
            for n in notes {
                if n.midi < midi_lo || n.midi > midi_hi || n.tick >= pat_ticks {
                    continue;
                }
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x_of(n.tick), y_of(n.midi) + 1.0),
                        egui::pos2(
                            x_of((n.tick + n.len).min(pat_ticks)),
                            y_of(n.midi) + cell_h - 1.0,
                        ),
                    ),
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(120, 120, 120, 60),
                );
            }
        }
    }
    // Notes.
    let notes_snapshot = s
        .song
        .patterns
        .get(pat)
        .and_then(|p| p.notes.get(ch))
        .cloned()
        .unwrap_or_default();
    for (idx, n) in notes_snapshot.iter().enumerate() {
        if n.midi < midi_lo || n.midi > midi_hi || n.tick >= pat_ticks {
            continue;
        }
        let col = if Some(idx) == ui.sel_note {
            egui::Color32::YELLOW
        } else {
            let g = (120.0 + n.vel * 120.0) as u8;
            egui::Color32::from_rgb(60, g, 90)
        };
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x_of(n.tick), y_of(n.midi) + 1.0),
                egui::pos2(
                    x_of((n.tick + n.len).min(pat_ticks)),
                    y_of(n.midi) + cell_h - 1.0,
                ),
            ),
            2.0,
            col,
        );
    }
    // Playhead.
    if s.rt.playing && s.rt.cur_pat == pat {
        let x = x_of(s.rt.cur_tick.min(pat_ticks - 1));
        painter.line_segment(
            [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
            egui::Stroke::new(1.5_f32, egui::Color32::LIGHT_GREEN),
        );
    }

    // Interactions.
    let hit_note = |tick: i32, midi: i32| -> Option<usize> {
        notes_snapshot.iter().rposition(|n| {
            tick >= n.tick as i32 && tick < (n.tick + n.len) as i32 && midi == n.midi as i32
        })
    };
    let origin = u.ctx().input(|i| i.pointer.press_origin());
    let in_grid = |t: i32, m: i32| {
        (0..pat_ticks as i32).contains(&t) && m >= midi_lo as i32 && m <= midi_hi as i32
    };
    if resp.drag_started()
        && let Some(p0) = origin
    {
        let t0 = tick_at(p0.x);
        let m0 = midi_at(p0.y);
        if p0.x < rect.min.x + KEY_W {
            if m0 >= midi_lo as i32 && m0 <= midi_hi as i32 {
                s.live_note_ch(ch, m0 as u8);
            }
        } else if in_grid(t0, m0) {
            ui.press_cell = Some((t0, m0));
            if let Some(idx) = hit_note(t0, m0) {
                if ui.tool == PianoTool::Erase
                    && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                    && idx < notes.len()
                {
                    notes.remove(idx);
                    ui.sel_note = None;
                } else if ui.tool == PianoTool::Hear {
                    s.live_note_ch(ch, m0 as u8);
                } else if resp.hover_pos().is_some() {
                    let n = &notes_snapshot[idx];
                    let right_edge = x_of(n.tick + n.len);
                    let kind = if p0.x > right_edge - 7.0 {
                        DragKind::Resize
                    } else {
                        DragKind::Move
                    };
                    ui.drag = Some((idx, kind, t0 - n.tick as i32, m0 - n.midi as i32));
                    ui.sel_note = Some(idx);
                }
            }
        }
    }
    if resp.drag_stopped() {
        if ui.drag.is_none()
            && ui.tool == PianoTool::Draw
            && let Some((t0, m0)) = ui.press_cell
            && in_grid(t0, m0)
        {
            let snap_t =
                ((t0 / ui.snap.max(1) as i32) * ui.snap.max(1) as i32).clamp(0, max_tick) as u32;
            place_note(ui, s, ch, pat, snap_t, scale_checked(ui, m0 as u8));
        }
        ui.drag = None;
        ui.press_cell = None;
    } else if resp.dragged()
        && let Some((idx, kind, gt, gm)) = ui.drag
        && let Some(pos) = resp.hover_pos()
    {
        let t = tick_at(pos.x);
        let m = midi_at(pos.y);
        if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
            && let Some(n) = notes.get_mut(idx)
        {
            match kind {
                DragKind::Move => {
                    n.tick = (t - gt).clamp(0, max_tick) as u32;
                    let target = (m - gm).clamp(21, 108) as u8;
                    n.midi = if ui.snap_scale {
                        ui.scale.snap_midi(target, ui.root)
                    } else {
                        target
                    };
                }
                DragKind::Resize => {
                    n.len = (t - n.tick as i32 + 1).clamp(1, pat_ticks as i32) as u32;
                }
            }
        }
    } else if ui.tool == PianoTool::Erase
        && let Some(pos) = resp.hover_pos()
    {
        let t = tick_at(pos.x);
        let m = midi_at(pos.y);
        if in_grid(t, m)
            && let Some(idx) = hit_note(t, m)
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
            && idx < notes.len()
        {
            notes.remove(idx);
            ui.sel_note = None;
        }
    }
    if resp.clicked()
        && !resp.drag_stopped()
        && let Some(pos) = resp.hover_pos()
    {
        let t = tick_at(pos.x);
        let m = midi_at(pos.y);
        if pos.x < rect.min.x + KEY_W {
            if m >= midi_lo as i32 && m <= midi_hi as i32 {
                s.live_note_ch(ch, m as u8);
            }
        } else if in_grid(t, m) {
            match ui.tool {
                PianoTool::Draw => {
                    if let Some(idx) = hit_note(t, m) {
                        ui.sel_note = Some(idx);
                    } else {
                        let snap_t = ((t / ui.snap.max(1) as i32) * ui.snap.max(1) as i32)
                            .clamp(0, max_tick) as u32;
                        let mp = scale_checked(ui, m as u8);
                        place_note(ui, s, ch, pat, snap_t, mp);
                    }
                }
                PianoTool::Erase => {
                    if let Some(idx) = hit_note(t, m)
                        && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                        && idx < notes.len()
                    {
                        notes.remove(idx);
                        ui.sel_note = None;
                    }
                }
                PianoTool::Hear => {
                    s.live_note_ch(ch, m as u8);
                }
            }
        }
    }
    if resp.clicked_by(egui::PointerButton::Secondary)
        && let Some(pos) = resp.hover_pos()
    {
        let t = tick_at(pos.x);
        let m = midi_at(pos.y);
        if let Some(idx) = hit_note(t, m)
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
            && idx < notes.len()
        {
            notes.remove(idx);
            ui.sel_note = None;
        }
    }
    if resp.double_clicked()
        && let Some(pos) = resp.hover_pos()
    {
        let t = tick_at(pos.x);
        let m = midi_at(pos.y);
        if in_grid(t, m) {
            ui.prop_note = hit_note(t, m);
            ui.sel_note = ui.prop_note;
        }
    }
    if u.ctx().input(|i| i.key_pressed(egui::Key::Delete))
        && let Some(idx) = ui.sel_note.take()
        && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
        && idx < notes.len()
    {
        notes.remove(idx);
    }
    rect
}

fn note_dialog(ui: &mut AppUi, s: &mut AppState, ch: usize, pat: usize, u: &mut egui::Ui) {
    if let Some(idx) = ui.prop_note {
        let count = s.song.patterns[pat]
            .notes
            .get(ch)
            .map(|ns| ns.len())
            .unwrap_or(0);
        let pat_ticks = s.song.patterns[pat].len_ticks().max(1);
        if idx >= count {
            ui.prop_note = None;
        } else {
            let mut close = false;
            let mut remove = false;
            egui::Window::new("Note")
                .collapsible(false)
                .show(u.ctx(), |w| {
                    if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                        && let Some(n) = notes.get_mut(idx)
                    {
                        w.label(format!("{} @ tick {}", note_label(n.midi), n.tick));
                        w.add(egui::Slider::new(&mut n.vel, 0.05..=1.0).text("Velocity"));
                        w.add(egui::Slider::new(&mut n.pan, -1.0..=1.0).text("Pan"));
                        w.add(
                            egui::Slider::new(&mut n.fine_pitch, -12.0..=12.0).text("Fine pitch"),
                        );
                        let mut len = n.len;
                        w.add(egui::Slider::new(&mut len, 1..=pat_ticks).text("Length"));
                        n.len = len;
                        let mut midi = n.midi;
                        w.add(egui::Slider::new(&mut midi, 21..=108).text("Pitch"));
                        n.midi = midi;
                        if w.button("Delete note").clicked() {
                            remove = true;
                        }
                        if w.button("Close").clicked() {
                            close = true;
                        }
                    } else {
                        close = true;
                    }
                });
            if remove
                && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                && idx < notes.len()
            {
                notes.remove(idx);
                ui.sel_note = None;
            }
            if remove || close {
                ui.prop_note = None;
            }
        }
    }
}

fn velocity_lane(s: &mut AppState, ch: usize, pat: usize, u: &mut egui::Ui) {
    u.label("Velocity (drag bars)");
    let avail_w = u.available_width().max(300.0);
    let h = 56.0;
    let (rect, resp) = u.allocate_exact_size(egui::vec2(avail_w, h), egui::Sense::click_and_drag());
    let painter = u.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(16));
    let pat_ticks = s.song.patterns[pat].len_ticks().max(1);
    let notes = s
        .song
        .patterns
        .get(pat)
        .and_then(|p| p.notes.get(ch))
        .cloned()
        .unwrap_or_default();
    for n in &notes {
        if n.tick >= pat_ticks {
            continue;
        }
        let x =
            rect.min.x + KEY_W + (n.tick as f32 + 0.5) / pat_ticks as f32 * (rect.width() - KEY_W);
        let bh = n.vel.clamp(0.0, 1.0) * (h - 6.0);
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(x - 3.0, rect.max.y - 3.0 - bh),
                egui::vec2(6.0, bh),
            ),
            1.0,
            egui::Color32::from_rgb(90, 160, 120),
        );
    }
    if resp.dragged()
        && let Some(pos) = resp.hover_pos()
    {
        let tick = (((pos.x - rect.min.x - KEY_W) / (rect.width() - KEY_W) * pat_ticks as f32)
            .round() as i32)
            .clamp(0, pat_ticks as i32 - 1);
        let vel = (1.0 - (pos.y - rect.min.y) / h).clamp(0.05, 1.0);
        if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
            let mut best: Option<usize> = None;
            let mut best_d = 3i32;
            for (idx, n) in notes.iter().enumerate() {
                let d = (n.tick as i32 - tick).abs();
                if d < best_d {
                    best_d = d;
                    best = Some(idx);
                }
            }
            if let Some(idx) = best {
                notes[idx].vel = vel;
            }
        }
    }
}

fn automation_lane(
    ui: &mut AppUi,
    s: &mut AppState,
    ch: usize,
    pat: usize,
    u: &mut egui::Ui,
) -> egui::Rect {
    u.horizontal(|u| {
        u.label("Automation:");
        u.selectable_value(&mut ui.auto_sel, AutoSel::Vol, "Volume");
        u.selectable_value(&mut ui.auto_sel, AutoSel::Cut, "Cutoff");
        u.label("(click: add · drag: move · right-click: delete)");
    });
    let is_vol = ui.auto_sel == AutoSel::Vol;
    let from_norm = |y: f32| {
        if is_vol {
            y.clamp(0.0, 1.0) * 1.5
        } else {
            (80.0_f32.ln() + y.clamp(0.0, 1.0) * (12000.0_f32.ln() - 80.0_f32.ln())).exp()
        }
    };
    let avail_w = u.available_width().max(300.0);
    let h = 72.0;
    let (rect, resp) = u.allocate_exact_size(egui::vec2(avail_w, h), egui::Sense::click_and_drag());
    let painter = u.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(16));
    let pat_ticks = s.song.patterns[pat].len_ticks().max(1);
    let points: Vec<AutoPoint> = {
        let pdata = &s.song.patterns[pat];
        (if is_vol {
            &pdata.auto_vol
        } else {
            &pdata.auto_cut
        })
        .get(ch)
        .cloned()
        .unwrap_or_default()
    };
    let x_of =
        |tick: u32| rect.min.x + KEY_W + tick as f32 / pat_ticks as f32 * (rect.width() - KEY_W);
    let y_of = |v: f32| {
        let norm = if is_vol {
            (v / 1.5).clamp(0.0, 1.0)
        } else {
            ((v.max(80.0).ln() - 80.0_f32.ln()) / (12000.0_f32.ln() - 80.0_f32.ln()))
                .clamp(0.0, 1.0)
        };
        rect.max.y - 4.0 - norm * (h - 8.0)
    };
    // Curve preview.
    let mut prev: Option<egui::Pos2> = None;
    for t in 0..pat_ticks {
        let v = eval_auto(&points, t).unwrap_or(if is_vol {
            1.0
        } else {
            s.song.channels[ch].cutoff
        });
        let p = egui::pos2(x_of(t), y_of(v));
        if let Some(pp) = prev {
            painter.line_segment(
                [pp, p],
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(150, 120, 220)),
            );
        }
        prev = Some(p);
    }
    for p in &points {
        painter.circle_filled(
            egui::pos2(x_of(p.tick), y_of(p.value)),
            4.0,
            egui::Color32::YELLOW,
        );
    }
    let tick_at = |x: f32| {
        (((x - rect.min.x - KEY_W) / (rect.width() - KEY_W) * pat_ticks as f32).round() as i32)
            .clamp(0, pat_ticks as i32 - 1) as u32
    };
    let val_at = |y: f32| from_norm(1.0 - (y - rect.min.y) / h);
    let near_idx = |points: &[AutoPoint], pos: egui::Pos2| -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut best_d = f32::MAX;
        for (idx, p) in points.iter().enumerate() {
            let dx = x_of(p.tick) - pos.x;
            let dy = y_of(p.value) - pos.y;
            let d = dx * dx + dy * dy;
            if d < best_d && d < 144.0 {
                best_d = d;
                best = Some(idx);
            }
        }
        best
    };
    if resp.drag_started() {
        ui.auto_drag = None;
        if let Some(p0) = u.ctx().input(|i| i.pointer.press_origin()) {
            let t = tick_at(p0.x);
            if near_idx(&points, p0).is_none() {
                let pts = if is_vol {
                    &mut s.song.patterns[pat].auto_vol
                } else {
                    &mut s.song.patterns[pat].auto_cut
                };
                if let Some(list) = pts.get_mut(ch) {
                    list.push(AutoPoint {
                        tick: t,
                        value: val_at(p0.y),
                    });
                    ui.auto_drag = Some(list.len() - 1);
                }
            } else {
                ui.auto_drag = near_idx(&points, p0);
            }
        }
    }
    if resp.drag_stopped() {
        ui.auto_drag = None;
    } else if resp.dragged()
        && let Some(idx) = ui.auto_drag
        && let Some(pos) = resp.hover_pos()
    {
        let pts = if is_vol {
            &mut s.song.patterns[pat].auto_vol
        } else {
            &mut s.song.patterns[pat].auto_cut
        };
        if let Some(list) = pts.get_mut(ch)
            && let Some(p) = list.get_mut(idx)
        {
            p.tick = tick_at(pos.x);
            p.value = val_at(pos.y);
        }
    }
    if resp.clicked()
        && !resp.drag_stopped()
        && let Some(pos) = resp.hover_pos()
    {
        let t = tick_at(pos.x);
        let pts = if is_vol {
            &mut s.song.patterns[pat].auto_vol
        } else {
            &mut s.song.patterns[pat].auto_cut
        };
        if let Some(list) = pts.get_mut(ch) {
            list.push(AutoPoint {
                tick: t,
                value: val_at(pos.y),
            });
            ui.status = "automation point added".into();
        }
    }
    if resp.clicked_by(egui::PointerButton::Secondary)
        && let Some(pos) = resp.hover_pos()
        && let Some(idx) = near_idx(&points, pos)
    {
        let pts = if is_vol {
            &mut s.song.patterns[pat].auto_vol
        } else {
            &mut s.song.patterns[pat].auto_cut
        };
        if let Some(list) = pts.get_mut(ch)
            && idx < list.len()
        {
            list.remove(idx);
        }
    }
    rect
}

// ---------------------------------------------------------------------------
// Tutorial
// ---------------------------------------------------------------------------

/// First-run walkthrough. Opens on the very first launch, then stays hidden
/// unless reopened from the toolbar. "Load demo" fills the blank project
/// with the example groove so new users hear something immediately.
pub fn tutorial(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    if !ui.show_tutorial {
        return;
    }
    let mut close = false;
    egui::Window::new("Welcome to Thunder")
        .collapsible(false)
        .show(u.ctx(), |w| {
            w.label("Make your first beat in two minutes:");
            w.label("1. Channel rack: click the numbered boxes to place kick, snare, hat hits.");
            w.label("2. Press Play (or Spacebar) to hear your pattern loop.");
            w.label("3. Click Bass or Lead, then click in the piano roll to draw notes.");
            w.label("4. Playlist: add bars and pick a pattern per bar to build a song.");
            w.label("5. Mixer: set levels, pan, echo and reverb sends.");
            w.label("6. Transport: save the project, export a WAV mixdown or MIDI file.");
            w.horizontal(|w| {
                if w.button("Load demo song").clicked() {
                    s.apply_loaded_song(crate::song::demo_song());
                    ui.sel_ch = 4;
                    ui.sel_pat = 0;
                    s.set_pattern(0);
                    ui.reset_history(&s.song);
                    ui.status = "demo loaded: press Play".into();
                    close = true;
                }
                if w.button("Start blank").clicked() {
                    close = true;
                }
            });
            let mut show_on_start = !tutorial_seen();
            if w.checkbox(&mut show_on_start, "Show on startup").changed() {
                set_tutorial_seen(!show_on_start);
            }
        });
    if close {
        ui.show_tutorial = false;
    }
}

const TRACK_HDR_W: f32 = 160.0;
const TRACK_ROW_H: f32 = 34.0;

fn add_playlist_clip_at(ui: &mut AppUi, s: &mut AppState, track: usize, tick: u32) -> Option<u64> {
    let bar_ticks = s.song.time_sig.ticks_per_bar().max(1);
    let id = match ui.playlist_paint {
        PlaylistPaint::Pattern => {
            let pat = ui.sel_pat.min(s.song.patterns.len().saturating_sub(1));
            let id = s.song.add_pattern_clip(track, tick, pat);
            ui.status = format!("placed P{} clip on track {}", pat + 1, track + 1);
            id
        }
        PlaylistPaint::Audio => {
            if s.song.samples.is_empty() {
                ui.status = "import an audio sample in Browser first".into();
                return None;
            }
            let smp = ui.clip_sample.min(s.song.samples.len() - 1);
            let name = s.song.samples[smp].name.clone();
            let id = s.song.add_audio_clip(track, tick, smp, 1.0);
            ui.status = format!("placed audio '{name}' on track {}", track + 1);
            id
        }
        PlaylistPaint::AutoChVol => {
            let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
            let id = s
                .song
                .add_auto_clip(track, tick, bar_ticks, AutoTarget::ChannelVol(ch));
            ui.status = format!("added {} Vol automation clip", s.song.channels[ch].name);
            id
        }
        PlaylistPaint::AutoChCut => {
            let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
            let id = s
                .song
                .add_auto_clip(track, tick, bar_ticks, AutoTarget::ChannelCut(ch));
            ui.status = format!("added {} Cut automation clip", s.song.channels[ch].name);
            id
        }
        PlaylistPaint::AutoInsertVol => {
            let ins = ui
                .sel_insert
                .min(s.song.mixer_tracks.len().saturating_sub(1));
            let id = s
                .song
                .add_auto_clip(track, tick, bar_ticks, AutoTarget::InsertVol(ins));
            ui.status = format!(
                "added {} Vol automation clip",
                s.song.mixer_tracks[ins].name
            );
            id
        }
        PlaylistPaint::AutoMasterVol => {
            let id = s
                .song
                .add_auto_clip(track, tick, bar_ticks, AutoTarget::MasterVol);
            ui.status = "added Master Vol automation clip".into();
            id
        }
    };
    ui.sel_clip = Some(id);
    Some(id)
}

pub fn playlist(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.horizontal(|u| {
        u.heading("Playlist");
        if u.button("+ Track").clicked() {
            let idx = s.song.add_playlist_track();
            ui.sel_track = idx;
            ui.status = format!("added Track {}", idx + 1);
        }
        let mut bars_v = ui.playlist_bars_view;
        u.add(
            egui::DragValue::new(&mut bars_v)
                .range(4..=64)
                .prefix("View bars: "),
        );
        ui.playlist_bars_view = bars_v.max(4);
        egui::ComboBox::from_id_salt("pl_snap")
            .width(72.0)
            .selected_text(match ui.playlist_snap {
                16 => "Snap: Bar",
                4 => "Snap: Beat",
                _ => "Snap: Step",
            })
            .show_ui(u, |u| {
                u.selectable_value(&mut ui.playlist_snap, 16, "Bar");
                u.selectable_value(&mut ui.playlist_snap, 4, "Beat");
                u.selectable_value(&mut ui.playlist_snap, 1, "Step");
            });
        egui::ComboBox::from_id_salt("pl_paint")
            .selected_text(ui.playlist_paint.label())
            .show_ui(u, |u| {
                for p in [
                    PlaylistPaint::Pattern,
                    PlaylistPaint::Audio,
                    PlaylistPaint::AutoChVol,
                    PlaylistPaint::AutoChCut,
                    PlaylistPaint::AutoInsertVol,
                    PlaylistPaint::AutoMasterVol,
                ] {
                    u.selectable_value(&mut ui.playlist_paint, p, p.label());
                }
            });
        if ui.playlist_paint == PlaylistPaint::Audio && !s.song.samples.is_empty() {
            let mut smp = ui.clip_sample.min(s.song.samples.len() - 1);
            egui::ComboBox::from_id_salt("pl_audio_smp")
                .selected_text(s.song.samples[smp].name.clone())
                .show_ui(u, |u| {
                    for (idx, entry) in s.song.samples.iter().enumerate() {
                        u.selectable_value(&mut smp, idx, entry.name.clone());
                    }
                });
            ui.clip_sample = smp;
        }
    });

    if s.song.playlist_tracks.is_empty() {
        s.song.add_playlist_track();
    }
    ui.sel_track = ui.sel_track.min(s.song.playlist_tracks.len() - 1);

    let bar_ticks = s.song.time_sig.ticks_per_bar().max(1);
    let beat_ticks = s.song.time_sig.ticks_per_beat().max(1);
    let snap_ticks = match ui.playlist_snap {
        16 => bar_ticks,
        4 => beat_ticks,
        _ => TICKS_PER_STEP,
    };
    let end_bars = s.song.playlist_len_ticks().div_ceil(bar_ticks);
    let view_bars = ui.playlist_bars_view.max(end_bars.saturating_add(2)).max(4);
    let total_ticks = view_bars * bar_ticks;

    let ntracks = s.song.playlist_tracks.len();
    let total_h = ntracks as f32 * TRACK_ROW_H;
    let avail_w = u.available_width().max(380.0);
    let lane_w = (avail_w - TRACK_HDR_W).max(200.0);
    let tick_w = lane_w / total_ticks as f32;

    let ch_names: Vec<String> = s.song.channels.iter().map(|c| c.name.clone()).collect();
    let mix_names: Vec<String> = s.song.mixer_tracks.iter().map(|m| m.name.clone()).collect();

    let mut lane_rect = egui::Rect::NOTHING;
    let mut lane_resp: Option<egui::Response> = None;

    u.horizontal_top(|u| {
        // Left track headers.
        u.vertical(|u| {
            u.set_width(TRACK_HDR_W);
            for (ti, tr) in s.song.playlist_tracks.iter_mut().enumerate() {
                u.allocate_ui_with_layout(
                    egui::vec2(TRACK_HDR_W, TRACK_ROW_H),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |u| {
                        let (sw, _) =
                            u.allocate_exact_size(egui::vec2(6.0, 24.0), egui::Sense::hover());
                        u.painter_at(sw).rect_filled(sw, 1.0, color32(tr.color));
                        if u.selectable_label(ui.sel_track == ti, tr.name.clone())
                            .clicked()
                        {
                            ui.sel_track = ti;
                        }
                        u.toggle_value(&mut tr.mute, "M");
                        u.toggle_value(&mut tr.solo, "S");
                        let mode_lbl = match (tr.linked_channel, tr.linked_insert) {
                            (Some(c), _) => format!("C{}", c + 1),
                            (None, Some(m)) => format!("I{m}"),
                            (None, None) => "Free".into(),
                        };
                        egui::ComboBox::from_id_salt(format!("tr_mode_{ti}"))
                            .width(42.0)
                            .selected_text(mode_lbl)
                            .show_ui(u, |u| {
                                if u.selectable_label(
                                    tr.linked_channel.is_none() && tr.linked_insert.is_none(),
                                    "Free",
                                )
                                .clicked()
                                {
                                    tr.linked_channel = None;
                                    tr.linked_insert = None;
                                }
                                for (ci, cname) in ch_names.iter().enumerate() {
                                    if u.selectable_label(
                                        tr.linked_channel == Some(ci),
                                        format!("Ch: {cname}"),
                                    )
                                    .clicked()
                                    {
                                        tr.linked_channel = Some(ci);
                                    }
                                }
                                for (mi, mname) in mix_names.iter().enumerate() {
                                    if u.selectable_label(
                                        tr.linked_insert == Some(mi),
                                        format!("Mix: {mname}"),
                                    )
                                    .clicked()
                                    {
                                        tr.linked_insert = Some(mi);
                                    }
                                }
                            });
                    },
                );
            }
        });

        // Right clip timeline canvas.
        let (rect, resp) =
            u.allocate_exact_size(egui::vec2(lane_w, total_h), egui::Sense::click_and_drag());
        lane_rect = rect;
        let painter = u.painter_at(rect);
        painter.rect_filled(rect, 2.0, egui::Color32::from_gray(16));

        for ti in 0..ntracks {
            let y0 = rect.min.y + ti as f32 * TRACK_ROW_H;
            let row_col = if ti % 2 == 0 {
                egui::Color32::from_gray(20)
            } else {
                egui::Color32::from_gray(24)
            };
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(rect.min.x, y0),
                    egui::vec2(lane_w, TRACK_ROW_H),
                ),
                0.0,
                row_col,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.min.x, y0 + TRACK_ROW_H),
                    egui::pos2(rect.max.x, y0 + TRACK_ROW_H),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_gray(35)),
            );
        }

        for bar in 0..=view_bars {
            let x = rect.min.x + (bar * bar_ticks) as f32 * tick_w;
            painter.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(1.0, egui::Color32::from_gray(55)),
            );
        }

        // Draw clips.
        for clip in &s.song.clips {
            if clip.track >= ntracks {
                continue;
            }
            let x0 = rect.min.x + clip.start_tick as f32 * tick_w;
            let w = (clip.len_ticks as f32 * tick_w).max(10.0);
            let y0 = rect.min.y + clip.track as f32 * TRACK_ROW_H + 2.0;
            let h = TRACK_ROW_H - 4.0;
            let crect = egui::Rect::from_min_size(egui::pos2(x0, y0), egui::vec2(w, h));
            let base_col = match &clip.kind {
                ClipKind::Pattern { pattern } => s
                    .song
                    .patterns
                    .get(*pattern)
                    .map(|p| color32(p.color))
                    .unwrap_or_else(|| color32(palette_color(*pattern))),
                ClipKind::Audio { .. } => egui::Color32::from_rgb(75, 145, 195),
                ClipKind::Automation { .. } => egui::Color32::from_rgb(175, 110, 200),
            };
            let fill = if clip.mute {
                egui::Color32::from_gray(65)
            } else if let Some(c) = clip.color {
                color32(c)
            } else {
                base_col
            };
            painter.rect_filled(crect, 3.0, fill);
            if Some(clip.id) == ui.sel_clip {
                painter.rect_stroke(
                    crect,
                    3.0,
                    egui::Stroke::new(2.0, egui::Color32::YELLOW),
                    egui::StrokeKind::Outside,
                );
            }
            let label = match &clip.kind {
                ClipKind::Pattern { pattern } => s
                    .song
                    .patterns
                    .get(*pattern)
                    .map(|p| format!("P{}: {}", pattern + 1, p.name))
                    .unwrap_or_else(|| format!("P{}", pattern + 1)),
                ClipKind::Audio { sample, .. } => s
                    .song
                    .samples
                    .get(*sample)
                    .map(|smp| smp.name.clone())
                    .unwrap_or_else(|| "Audio".into()),
                ClipKind::Automation { target, points } => {
                    // Mini automation curve preview.
                    let (lo, hi) = target.range();
                    let span = (hi - lo).max(1e-4);
                    let mut prev: Option<egui::Pos2> = None;
                    for pt in points {
                        let px = crect.min.x
                            + (pt.tick as f32 / clip.len_ticks.max(1) as f32) * crect.width();
                        let py = crect.max.y
                            - 3.0
                            - ((pt.value - lo) / span).clamp(0.0, 1.0) * (h - 6.0);
                        let p = egui::pos2(px, py);
                        if let Some(pp) = prev {
                            painter.line_segment(
                                [pp, p],
                                egui::Stroke::new(1.5, egui::Color32::WHITE),
                            );
                        }
                        prev = Some(p);
                    }
                    target.label()
                }
            };
            painter.text(
                egui::pos2(crect.min.x + 4.0, crect.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(11.0),
                egui::Color32::BLACK,
            );
        }

        if s.rt.playing && s.rt.song_mode {
            let px = rect.min.x + (s.rt.tick_pos.max(0.0) as f32 * tick_w);
            painter.line_segment(
                [egui::pos2(px, rect.min.y), egui::pos2(px, rect.max.y)],
                egui::Stroke::new(1.5, egui::Color32::LIGHT_GREEN),
            );
        }

        lane_resp = Some(resp);
    });

    fn hit_clip_at(
        clips: &[crate::song::Clip],
        lane_rect: egui::Rect,
        ntracks: usize,
        tick_w: f32,
        pos: egui::Pos2,
    ) -> Option<(usize, f32)> {
        let track = ((pos.y - lane_rect.min.y) / TRACK_ROW_H).floor() as isize;
        if track < 0 || track as usize >= ntracks {
            return None;
        }
        let tick = (pos.x - lane_rect.min.x) / tick_w;
        clips.iter().enumerate().rev().find_map(|(idx, c)| {
            if c.track == track as usize
                && tick >= c.start_tick as f32
                && tick <= (c.start_tick + c.len_ticks) as f32
            {
                let right_px = lane_rect.min.x + (c.start_tick + c.len_ticks) as f32 * tick_w;
                Some((idx, right_px - pos.x))
            } else {
                None
            }
        })
    }

    if let Some(resp) = lane_resp {
        let origin = u.ctx().input(|i| i.pointer.press_origin());
        if resp.drag_started()
            && let Some(p0) = origin
            && let Some((ci, right_dist)) =
                hit_clip_at(&s.song.clips, lane_rect, ntracks, tick_w, p0)
        {
            let c = &s.song.clips[ci];
            ui.sel_clip = Some(c.id);
            let tick0 = ((p0.x - lane_rect.min.x) / tick_w).floor() as i32;
            let kind = if right_dist <= 8.0 {
                ClipDragKind::Resize
            } else {
                ClipDragKind::Move
            };
            ui.clip_drag = Some((c.id, kind, tick0 - c.start_tick as i32, c.track as u32));
        }

        if resp.drag_stopped() {
            ui.clip_drag = None;
        } else if resp.dragged()
            && let Some((cid, kind, offset_t, _)) = ui.clip_drag
            && let Some(pos) = resp.hover_pos()
            && let Some(c) = s.song.clips.iter_mut().find(|c| c.id == cid)
        {
            let raw_t = ((pos.x - lane_rect.min.x) / tick_w).floor() as i32;
            match kind {
                ClipDragKind::Move => {
                    let snapped = ((raw_t - offset_t).max(0) as u32 / snap_ticks) * snap_ticks;
                    c.start_tick = snapped;
                    let tr = ((pos.y - lane_rect.min.y) / TRACK_ROW_H)
                        .floor()
                        .clamp(0.0, (ntracks - 1) as f32) as usize;
                    c.track = tr;
                    ui.sel_track = tr;
                }
                ClipDragKind::Resize => {
                    let new_end = ((raw_t.max(c.start_tick as i32 + 1) as u32)
                        .div_ceil(snap_ticks))
                        * snap_ticks;
                    c.len_ticks = (new_end.saturating_sub(c.start_tick)).max(TICKS_PER_STEP);
                }
            }
        }

        if resp.clicked()
            && !resp.drag_stopped()
            && let Some(pos) = resp.hover_pos()
        {
            if let Some((ci, _)) = hit_clip_at(&s.song.clips, lane_rect, ntracks, tick_w, pos) {
                let c = &s.song.clips[ci];
                ui.sel_clip = Some(c.id);
                ui.sel_track = c.track;
                if let ClipKind::Pattern { pattern } = c.kind {
                    ui.sel_pat = pattern.min(s.song.patterns.len().saturating_sub(1));
                    s.select_pattern(ui.sel_pat);
                }
            } else {
                let tr = ((pos.y - lane_rect.min.y) / TRACK_ROW_H)
                    .floor()
                    .clamp(0.0, (ntracks - 1) as f32) as usize;
                let raw_t = ((pos.x - lane_rect.min.x) / tick_w).max(0.0) as u32;
                let snapped = (raw_t / snap_ticks) * snap_ticks;
                ui.sel_track = tr;
                add_playlist_clip_at(ui, s, tr, snapped);
            }
        }

        if resp.double_clicked()
            && let Some(pos) = resp.hover_pos()
            && let Some((ci, _)) = hit_clip_at(&s.song.clips, lane_rect, ntracks, tick_w, pos)
            && let ClipKind::Pattern { pattern } = s.song.clips[ci].kind
        {
            ui.sel_pat = pattern.min(s.song.patterns.len().saturating_sub(1));
            s.select_pattern(ui.sel_pat);
            ui.show_piano = true;
            ui.status = format!("editing P{}", ui.sel_pat + 1);
        }

        if resp.clicked_by(egui::PointerButton::Secondary)
            && let Some(pos) = resp.hover_pos()
            && let Some((ci, _)) = hit_clip_at(&s.song.clips, lane_rect, ntracks, tick_w, pos)
        {
            let id = s.song.clips[ci].id;
            s.song.remove_clip(id);
            if ui.sel_clip == Some(id) {
                ui.sel_clip = None;
            }
            ui.status = "removed clip".into();
        }
    }

    // Selected clip inspector + Automation Clip curve editor.
    if let Some(cid) = ui.sel_clip {
        if let Some(idx) = s.song.clips.iter().position(|c| c.id == cid) {
            let mut del = false;
            let mut uniq = false;
            u.horizontal(|u| {
                u.label(format!("Clip #{cid}:"));
                u.toggle_value(&mut s.song.clips[idx].mute, "Mute");
                if matches!(s.song.clips[idx].kind, ClipKind::Pattern { .. })
                    && u.button("Make unique").clicked()
                {
                    uniq = true;
                }
                if u.button("Delete clip").clicked() {
                    del = true;
                }
                if let ClipKind::Audio {
                    gain,
                    pitch_semitones,
                    fade_in_ticks,
                    fade_out_ticks,
                    ..
                } = &mut s.song.clips[idx].kind
                {
                    u.add(egui::Slider::new(gain, 0.0..=2.0).text("Gain"));
                    u.add(egui::Slider::new(pitch_semitones, -24.0..=24.0).text("Pitch"));
                    u.add(
                        egui::DragValue::new(fade_in_ticks)
                            .range(0..=128)
                            .prefix("In: "),
                    );
                    u.add(
                        egui::DragValue::new(fade_out_ticks)
                            .range(0..=128)
                            .prefix("Out: "),
                    );
                }
            });
            if uniq && let Some(np) = s.song.make_clip_unique(cid) {
                ui.sel_pat = np;
                s.select_pattern(np);
                ui.status = format!("made clip unique -> P{}", np + 1);
            }
            if del {
                s.song.remove_clip(cid);
                ui.sel_clip = None;
                return;
            }
            if matches!(s.song.clips[idx].kind, ClipKind::Automation { .. }) {
                auto_clip_editor(ui, s, idx, u);
            }
        } else {
            ui.sel_clip = None;
        }
    }
}

fn auto_clip_editor(ui: &mut AppUi, s: &mut AppState, clip_idx: usize, u: &mut egui::Ui) {
    let cid = s.song.clips[clip_idx].id;
    let len_ticks = s.song.clips[clip_idx].len_ticks.max(1);
    let ClipKind::Automation { target, points } = &mut s.song.clips[clip_idx].kind else {
        return;
    };
    let (lo, hi) = target.range();
    let span = (hi - lo).max(1e-4);
    u.horizontal(|u| {
        u.label(format!("Automation clip ({})", target.label()));
        if let Some(last) = points.last_mut() {
            u.add(egui::Slider::new(&mut last.tension, -1.0..=1.0).text("Last tension"));
        }
        u.label("(click add · drag move · right-click remove)");
    });
    let avail_w = u.available_width().max(260.0);
    let h = 64.0;
    let (rect, resp) = u.allocate_exact_size(egui::vec2(avail_w, h), egui::Sense::click_and_drag());
    let painter = u.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(16));

    let x_of = |tick: u32| rect.min.x + (tick as f32 / len_ticks as f32) * rect.width();
    let y_of = |v: f32| {
        let norm = ((v - lo) / span).clamp(0.0, 1.0);
        rect.max.y - 4.0 - norm * (h - 8.0)
    };
    let tick_at = |x: f32| {
        (((x - rect.min.x) / rect.width()) * len_ticks as f32)
            .round()
            .clamp(0.0, len_ticks as f32) as u32
    };
    let val_at = |y: f32| {
        let norm = (1.0 - (y - rect.min.y) / h).clamp(0.0, 1.0);
        lo + norm * span
    };

    let mut prev: Option<egui::Pos2> = None;
    for step in 0..=48u32 {
        let t = step as f32 / 48.0 * len_ticks as f32;
        if let Some(v) = eval_auto_clip(points, t) {
            let p = egui::pos2(rect.min.x + (t / len_ticks as f32) * rect.width(), y_of(v));
            if let Some(pp) = prev {
                painter.line_segment(
                    [pp, p],
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(200, 130, 230)),
                );
            }
            prev = Some(p);
        }
    }
    for pt in points.iter() {
        painter.circle_filled(
            egui::pos2(x_of(pt.tick), y_of(pt.value)),
            4.0,
            egui::Color32::YELLOW,
        );
    }

    let near_idx = |pts: &[AutoClipPoint], pos: egui::Pos2| -> Option<usize> {
        pts.iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let dx = x_of(p.tick) - pos.x;
                let dy = y_of(p.value) - pos.y;
                let d = dx * dx + dy * dy;
                (d < 144.0).then_some((i, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    };

    if resp.drag_started()
        && let Some(p0) = u.ctx().input(|i| i.pointer.press_origin())
    {
        if let Some(idx) = near_idx(points, p0) {
            ui.playlist_auto_drag = Some((cid, idx));
        } else {
            points.push(AutoClipPoint {
                tick: tick_at(p0.x),
                value: val_at(p0.y),
                tension: 0.0,
            });
            points.sort_by_key(|p| p.tick);
            if let Some(idx) = near_idx(points, p0) {
                ui.playlist_auto_drag = Some((cid, idx));
            }
        }
    }
    if resp.drag_stopped() {
        ui.playlist_auto_drag = None;
    } else if resp.dragged()
        && let Some((dcid, idx)) = ui.playlist_auto_drag
        && dcid == cid
        && let Some(pos) = resp.hover_pos()
        && idx < points.len()
    {
        points[idx].tick = tick_at(pos.x);
        points[idx].value = val_at(pos.y);
        points.sort_by_key(|p| p.tick);
    }
    if resp.clicked()
        && !resp.drag_stopped()
        && let Some(pos) = resp.hover_pos()
        && near_idx(points, pos).is_none()
    {
        points.push(AutoClipPoint {
            tick: tick_at(pos.x),
            value: val_at(pos.y),
            tension: 0.0,
        });
        points.sort_by_key(|p| p.tick);
    }
    if resp.clicked_by(egui::PointerButton::Secondary)
        && let Some(pos) = resp.hover_pos()
        && points.len() > 2
        && let Some(idx) = near_idx(points, pos)
    {
        points.remove(idx);
    }
}

// ---------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------

pub fn try_add_mixer_route(
    ui: &mut AppUi,
    s: &mut AppState,
    from: usize,
    to: usize,
    amount: f32,
    sidechain_only: bool,
) -> bool {
    match s.song.add_mixer_route(from, to, amount, sidechain_only) {
        Ok(()) => {
            ui.status = format!(
                "routed {} -> {}{}",
                s.song.mixer_tracks[from].name,
                s.song.mixer_tracks[to].name,
                if sidechain_only { " (sidechain)" } else { "" }
            );
            true
        }
        Err(e) => {
            ui.status = e;
            false
        }
    }
}

pub fn mixer(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.horizontal(|u| {
        u.heading("Mixer");
        if u.button("+ Insert").clicked() {
            let idx = s.song.add_mixer_insert();
            ui.sel_insert = idx;
            ui.status = format!("added Insert {idx}");
        }
        if u.button("Link Ch (Ctrl+L)").clicked() {
            link_selected_channel_to_insert(ui, s);
        }
    });

    if s.song.mixer_tracks.is_empty() {
        return;
    }
    ui.sel_insert = ui.sel_insert.min(s.song.mixer_tracks.len() - 1);
    let pdc = s.song.compute_pdc();
    let linked_ins = s
        .song
        .channels
        .get(ui.sel_ch)
        .map(|c| c.mixer_insert)
        .unwrap_or(0);

    egui::ScrollArea::horizontal()
        .id_salt("mixer_strips_scroll")
        .show(u, |u| {
            u.horizontal(|u| {
                for i in 0..s.song.mixer_tracks.len() {
                    let is_sel = ui.sel_insert == i;
                    let is_linked = linked_ins == i;
                    u.group(|u| {
                        u.vertical(|u| {
                            let label = if is_linked {
                                format!("* {}", s.song.mixer_tracks[i].name)
                            } else {
                                s.song.mixer_tracks[i].name.clone()
                            };
                            if u.selectable_label(is_sel, label).clicked() {
                                ui.sel_insert = i;
                            }
                            let peak =
                                s.rt.mix_peaks
                                    .get(i)
                                    .copied()
                                    .unwrap_or(0.0)
                                    .clamp(0.0, 1.0);
                            let (mrect, _) =
                                u.allocate_exact_size(egui::vec2(58.0, 7.0), egui::Sense::hover());
                            let mp = u.painter_at(mrect);
                            mp.rect_filled(mrect, 1.0, egui::Color32::from_gray(28));
                            if peak > 0.003 {
                                let col = if peak > 0.9 {
                                    egui::Color32::from_rgb(220, 70, 60)
                                } else if peak > 0.65 {
                                    egui::Color32::from_rgb(220, 180, 60)
                                } else {
                                    egui::Color32::from_rgb(90, 200, 100)
                                };
                                mp.rect_filled(
                                    egui::Rect::from_min_size(
                                        mrect.min,
                                        egui::vec2(mrect.width() * peak, 7.0),
                                    ),
                                    1.0,
                                    col,
                                );
                            }
                            u.horizontal(|u| {
                                u.toggle_value(&mut s.song.mixer_tracks[i].mute, "M");
                                u.toggle_value(&mut s.song.mixer_tracks[i].solo, "S");
                            });
                            u.add(
                                egui::Slider::new(&mut s.song.mixer_tracks[i].volume, 0.0..=1.5)
                                    .vertical()
                                    .text("vol"),
                            );
                            u.add(
                                egui::Slider::new(&mut s.song.mixer_tracks[i].pan, -1.0..=1.0)
                                    .text("pan"),
                            );
                            u.add(
                                egui::Slider::new(
                                    &mut s.song.mixer_tracks[i].delay_send,
                                    0.0..=1.0,
                                )
                                .text("dly"),
                            );
                            u.add(
                                egui::Slider::new(
                                    &mut s.song.mixer_tracks[i].reverb_send,
                                    0.0..=1.0,
                                )
                                .text("vrb"),
                            );
                            let total_lat = pdc.total_out.get(i).copied().unwrap_or(0);
                            u.label(format!("pdc:{total_lat}"));
                        });
                    });
                }
                u.separator();
                u.vertical(|u| {
                    u.label("Send FX");
                    u.add(egui::Slider::new(&mut s.song.delay_time, 0.05..=1.0).text("dly t"));
                    u.add(egui::Slider::new(&mut s.song.delay_mix, 0.0..=0.8).text("dly x"));
                    u.add(egui::Slider::new(&mut s.song.reverb_mix, 0.0..=0.8).text("vrb x"));
                });
            });
        });

    // Selected insert routing + PDC inspector.
    let ins = ui.sel_insert;
    let names: Vec<String> = s.song.mixer_tracks.iter().map(|t| t.name.clone()).collect();
    u.group(|u| {
        u.horizontal(|u| {
            u.label(format!("Routing: {}", s.song.mixer_tracks[ins].name));
            u.add(
                egui::TextEdit::singleline(&mut s.song.mixer_tracks[ins].name).desired_width(90.0),
            );
            u.add(
                egui::DragValue::new(&mut s.song.mixer_tracks[ins].latency_samples)
                    .range(0..=4096)
                    .prefix("Lat: ")
                    .suffix(" smp"),
            );
            let comp = pdc.direct_delay.get(ins).copied().unwrap_or(0);
            let out_lat = pdc.total_out.get(ins).copied().unwrap_or(0);
            u.label(format!("PDC: +{comp} smp, out {out_lat} smp"));
        });
        if ins > 0 {
            let mut rm_target: Option<usize> = None;
            u.horizontal_wrapped(|u| {
                for r in &mut s.song.mixer_tracks[ins].routes {
                    let tname = names
                        .get(r.target)
                        .cloned()
                        .unwrap_or_else(|| format!("I{}", r.target));
                    u.label(format!("-> {tname}"));
                    u.add(egui::Slider::new(&mut r.amount, 0.0..=1.5).show_value(false));
                    u.toggle_value(&mut r.sidechain_only, "SC");
                    if u.small_button("×").clicked() {
                        rm_target = Some(r.target);
                    }
                }
            });
            if let Some(t) = rm_target {
                s.song.remove_mixer_route(ins, t);
            }
            u.horizontal(|u| {
                let mut tgt = ui.route_target.min(names.len().saturating_sub(1));
                egui::ComboBox::from_id_salt("mix_route_tgt")
                    .selected_text(names.get(tgt).cloned().unwrap_or_default())
                    .show_ui(u, |u| {
                        for (mi, nm) in names.iter().enumerate() {
                            if mi != ins {
                                u.selectable_value(&mut tgt, mi, nm.clone());
                            }
                        }
                    });
                ui.route_target = tgt;
                u.add(egui::Slider::new(&mut ui.route_amount, 0.0..=1.5).text("Amt"));
                u.toggle_value(&mut ui.route_sidechain, "Sidechain");
                if u.button("+ Route").clicked() {
                    let amt = ui.route_amount;
                    let sc = ui.route_sidechain;
                    try_add_mixer_route(ui, s, ins, tgt, amt, sc);
                }
            });
        }
    });

    // Scope.
    let scope = s.rt.scope_ordered();
    let (rect, _) = u.allocate_exact_size(
        egui::vec2(u.available_width().max(200.0), 80.0),
        egui::Sense::hover(),
    );
    let painter = u.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(18));
    let n = scope.len();
    let mut prev: Option<egui::Pos2> = None;
    for (k, v) in scope.iter().enumerate() {
        let vv = v.clamp(-1.0, 1.0);
        let x = rect.min.x + rect.width() * (k as f32 / n as f32);
        let y = rect.center().y - vv * rect.height() * 0.45;
        let p = egui::pos2(x, y);
        if let Some(pp) = prev {
            painter.line_segment(
                [pp, p],
                egui::Stroke::new(1.0_f32, egui::Color32::LIGHT_GREEN),
            );
        }
        prev = Some(p);
    }
}

// ---------------------------------------------------------------------------
// Sample browser
// ---------------------------------------------------------------------------

pub fn browser(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.heading("Browser / sampler");
    u.horizontal(|u| {
        u.label("Dir:");
        if u.add(egui::TextEdit::singleline(&mut ui.browser_dir).desired_width(200.0))
            .changed()
        {
            ui.browser_cache = sample_files(&ui.browser_dir);
        }
        if u.button("↻").clicked() {
            ui.browser_cache = sample_files(&ui.browser_dir);
        }
        if u.button("~/Music").clicked() {
            ui.browser_dir = home_music_dir();
            ui.browser_cache = sample_files(&ui.browser_dir);
        }
    });
    if ui.browser_cache.is_empty() {
        ui.browser_cache = sample_files(&ui.browser_dir);
    }
    for path in ui.browser_cache.clone() {
        u.horizontal(|u| {
            let name = std::path::Path::new(&path)
                .file_name()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            u.label(name);
            if u.button("▶").on_hover_text("Preview").clicked() {
                match sample_into_song(s, &path) {
                    Ok(idx) => {
                        s.live_sample(idx);
                        ui.status = format!("preview {path}");
                    }
                    Err(e) => ui.status = format!("preview failed: {e}"),
                }
            }
            if u.button("→ Ch")
                .on_hover_text("Load into selected channel")
                .clicked()
            {
                match sample_into_song(s, &path) {
                    Ok(idx) => {
                        s.song.channels[ui.sel_ch].kind =
                            crate::song::ChannelKind::Sampler { sample: Some(idx) };
                        ui.status = format!("loaded to channel: {path}");
                    }
                    Err(e) => ui.status = format!("load failed: {e}"),
                }
            }
        });
    }
    u.horizontal(|u| {
        u.label("Path:");
        u.add(egui::TextEdit::singleline(&mut ui.sample_path).desired_width(220.0));
        if u.button("Load → Ch").clicked() {
            let path = ui.sample_path.clone();
            match sample_into_song(s, &path) {
                Ok(idx) => {
                    s.song.channels[ui.sel_ch].kind =
                        crate::song::ChannelKind::Sampler { sample: Some(idx) };
                    ui.status = format!("loaded to channel: {path}");
                }
                Err(e) => ui.status = format!("load failed: {e}"),
            }
        }
    });
    u.label(format!("{} samples in project", s.song.samples.len()));
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::engine::AppState;
    use egui_kittest::{Harness, kittest::Queryable};

    struct TestState {
        ui: AppUi,
        app: AppState,
        rect: Option<egui::Rect>,
    }

    fn piano_harness() -> Harness<'static, TestState> {
        let state = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                let r = piano_canvas(&mut st.ui, &mut st.app, 4, 0, u);
                st.rect = Some(r);
            },
            state,
        );
        h.set_size(egui::vec2(1200.0, 800.0));
        h.step();
        h
    }

    fn cell_center(h: &Harness<'_, TestState>, tick: u32, midi: u8) -> egui::Pos2 {
        let rect = h.state().rect.expect("canvas rect recorded");
        let cell_w = (rect.width() - KEY_W) / 64.0;
        let (_, midi_hi) = piano_midi_range(40);
        egui::pos2(
            rect.min.x + KEY_W + (tick as f32 + 0.5) * cell_w,
            rect.min.y + (midi_hi - midi) as f32 * 9.0 + 4.5,
        )
    }

    fn click_at(h: &mut Harness<'_, TestState>, pos: egui::Pos2) {
        use egui::PointerButton;
        h.hover_at(pos);
        h.event(egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
        h.event(egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }

    #[test]
    fn piano_click_adds_note() {
        let mut h = piano_harness();
        assert!(h.state().app.song.patterns[0].notes[4].is_empty());
        let target = cell_center(&h, 8, 60);
        click_at(&mut h, target);
        let notes = &h.state().app.song.patterns[0].notes[4];
        assert_eq!(notes.len(), 1, "click must place exactly one note");
        assert_eq!(notes[0].tick, 8);
        assert_eq!(notes[0].midi, 60);
    }

    #[test]
    fn piano_drag_moves_note() {
        let mut state = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        state.app.song.patterns[0].notes[4].push(Note::new(8, 4, 60, 0.9));
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                let r = piano_canvas(&mut st.ui, &mut st.app, 4, 0, u);
                st.rect = Some(r);
            },
            state,
        );
        h.set_size(egui::vec2(1200.0, 800.0));
        h.step();
        let from = cell_center(&h, 8, 60);
        let to = cell_center(&h, 16, 60);
        h.hover_at(from);
        h.drag_at(from);
        h.step();
        h.hover_at(to);
        h.step();
        h.drop_at(to);
        h.step();
        let notes = &h.state().app.song.patterns[0].notes[4];
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].tick, 16, "drag must move the note");
        assert_eq!(notes[0].midi, 60);
    }

    #[test]
    fn piano_drag_resizes_note() {
        let mut state = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        state.app.song.patterns[0].notes[4].push(Note::new(8, 4, 60, 0.9));
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                let r = piano_canvas(&mut st.ui, &mut st.app, 4, 0, u);
                st.rect = Some(r);
            },
            state,
        );
        h.set_size(egui::vec2(1200.0, 800.0));
        h.step();
        let rect = h.state().rect.expect("canvas rect recorded");
        let cell_w = (rect.width() - KEY_W) / 64.0;
        let (_, midi_hi) = piano_midi_range(40);
        let y = rect.min.y + (midi_hi - 60) as f32 * 9.0 + 4.5;
        let from = egui::pos2(rect.min.x + KEY_W + 12.0 * cell_w - 3.0, y);
        let to = egui::pos2(rect.min.x + KEY_W + 16.5 * cell_w, y);
        h.hover_at(from);
        h.drag_at(from);
        h.step();
        h.hover_at(to);
        h.step();
        h.drop_at(to);
        h.step();
        let notes = &h.state().app.song.patterns[0].notes[4];
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].tick, 8, "resize must not move the note");
        assert_eq!(notes[0].len, 9, "resize must extend the note");
    }

    #[test]
    fn song_history_undoes_and_redoes_an_edit() {
        let mut ui = AppUi::default();
        let mut song = crate::song::new_empty();
        ui.record_song(&song, false);
        song.bpm = 140.0;
        ui.record_song(&song, false);

        assert!(ui.undo_song(&mut song));
        assert_eq!(song.bpm, 128.0);
        assert!(ui.redo_song(&mut song));
        assert_eq!(song.bpm, 140.0);
    }

    #[test]
    fn song_history_coalesces_a_drag() {
        let mut ui = AppUi::default();
        let mut song = crate::song::new_empty();
        ui.record_song(&song, false);
        song.bpm = 130.0;
        ui.record_song(&song, true);
        song.bpm = 140.0;
        ui.record_song(&song, true);
        ui.record_song(&song, false);

        assert!(ui.undo_song(&mut song));
        assert_eq!(song.bpm, 128.0);
        assert!(!ui.undo_song(&mut song));
    }

    #[test]
    fn copy_paste_notes_between_patterns() {
        let mut ui = AppUi::default();
        let mut state = AppState::new();
        state.song.patterns[0].notes[4].push(Note::new(8, 4, 60, 0.8));
        ui.sel_ch = 4;
        ui.sel_pat = 0;

        assert!(copy_notes(&mut ui, &state));
        ui.sel_pat = 1;
        assert!(paste_notes(&mut ui, &mut state));
        assert_eq!(state.song.patterns[1].notes[4][0].midi, 60);
    }

    #[test]
    fn duplicate_pattern_keeps_notes_and_automation() {
        let mut ui = AppUi::default();
        let mut state = AppState::new();
        state.song.patterns[0].notes[4].push(Note::new(8, 4, 60, 0.8));
        state.song.patterns[0].auto_vol[4].push(AutoPoint {
            tick: 8,
            value: 0.5,
        });

        assert!(duplicate_pattern(&mut ui, &mut state));
        assert_eq!(ui.sel_pat, 1);
        assert_eq!(state.song.patterns[1].notes[4].len(), 1);
        assert_eq!(state.song.patterns[1].auto_vol[4].len(), 1);
    }

    #[test]
    fn playlist_places_and_removes_audio_clip() {
        let mut ui = AppUi::default();
        let mut state = AppState::new();
        state.song.samples.push(crate::song::SampleRef {
            name: "tone.wav".into(),
            path: "tone.wav".into(),
            data: vec![0.5].into(),
            sr: 44100.0,
        });
        ui.playlist_paint = PlaylistPaint::Audio;
        ui.clip_sample = 0;
        let id = add_playlist_clip_at(&mut ui, &mut state, 1, 64).unwrap();
        assert_eq!(state.song.clips.len(), 1);
        assert!(state.song.remove_clip(id));
        assert!(state.song.clips.is_empty());
    }

    #[test]
    fn channel_rack_adds_and_links_channel() {
        struct CState {
            ui: AppUi,
            app: AppState,
        }
        let state = CState {
            ui: AppUi::default(),
            app: AppState::new(),
        };
        let initial = state.app.song.channels.len();
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut CState| {
                channel_rack(&mut st.ui, &mut st.app, u);
            },
            state,
        );
        h.set_size(egui::vec2(1200.0, 600.0));
        h.step();
        h.get_by_label("+ Synth").click();
        h.step();
        assert_eq!(h.state().app.song.channels.len(), initial + 1);
        let st = h.state_mut();
        st.ui.sel_insert = 3;
        assert!(link_selected_channel_to_insert(&mut st.ui, &mut st.app));
        assert_eq!(st.app.song.channels[st.ui.sel_ch].mixer_insert, 3);
    }

    #[test]
    fn mixer_ui_rejects_route_cycle_and_adds_insert() {
        let mut ui = AppUi::default();
        let mut state = AppState::new();
        let before = state.song.mixer_tracks.len();
        let ins = state.song.add_mixer_insert();
        assert_eq!(ins, before);
        state.song.remove_mixer_route(1, 0);
        assert!(try_add_mixer_route(&mut ui, &mut state, 1, 2, 1.0, false));
        assert!(!try_add_mixer_route(&mut ui, &mut state, 2, 1, 1.0, false));
        assert!(ui.status.contains("cycle"));
    }

    #[test]
    fn automation_drag_moves_point() {
        use crate::song::AutoPoint;
        struct AState {
            ui: AppUi,
            app: AppState,
            rect: Option<egui::Rect>,
        }
        let mut state = AState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        state.app.song.patterns[0].auto_vol[4].push(AutoPoint {
            tick: 16,
            value: 1.0,
        });
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut AState| {
                let r = automation_lane(&mut st.ui, &mut st.app, 4, 0, u);
                st.rect = Some(r);
            },
            state,
        );
        h.set_size(egui::vec2(1200.0, 800.0));
        h.step();
        let rect = h.state().rect.expect("lane rect recorded");
        let x_of = |tick: u32| rect.min.x + KEY_W + tick as f32 / 64.0 * (rect.width() - KEY_W);
        let y = rect.max.y - 4.0 - (1.0 / 1.5) * (72.0 - 8.0);
        let from = egui::pos2(x_of(16), y);
        let to = egui::pos2(x_of(32), y);
        h.hover_at(from);
        h.drag_at(from);
        h.step();
        h.hover_at(to);
        h.step();
        h.drop_at(to);
        h.step();
        let pts = &h.state().app.song.patterns[0].auto_vol[4];
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].tick, 32, "drag must move the automation point");
    }

    #[test]
    fn transport_rec_arms_and_plays() {
        struct TState {
            ui: AppUi,
            app: AppState,
        }
        let state = TState {
            ui: AppUi::default(),
            app: AppState::new(),
        };
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TState| {
                transport(&mut st.ui, &mut st.app, u);
            },
            state,
        );
        assert!(!h.state().app.rt.recording);
        h.get_by_label("\u{25cf} Rec").click();
        h.step();
        assert!(h.state().app.rt.recording, "Rec must arm recording");
        assert!(h.state().app.rt.playing, "Rec must start transport");
        assert!(h.state().app.rt.take_l.is_empty(), "new take starts empty");
    }

    #[test]
    fn transport_play_toggles() {
        struct TState {
            ui: AppUi,
            app: AppState,
        }
        let state = TState {
            ui: AppUi::default(),
            app: AppState::new(),
        };
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TState| {
                transport(&mut st.ui, &mut st.app, u);
            },
            state,
        );
        assert!(!h.state().app.rt.playing);
        h.get_by_label("▶ Play").click();
        h.step();
        assert!(h.state().app.rt.playing, "Play must start transport");
    }
}
