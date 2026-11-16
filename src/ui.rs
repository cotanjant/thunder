//! All egui panels: transport, channel rack, piano roll + automation,
//! playlist, mixer, sample browser.

use crate::clap_host::{
    ClapAudioNode, ClapCatalog, ClapManager, ClapPluginDesc, default_clap_dirs,
    load_cached_catalog, save_cached_catalog, scan_clap_dirs, scan_clap_file_isolated,
};
use crate::engine::{AppState, sample_into_song};
use crate::export::{ExportConfig, ExportFormat, TailMode};
use crate::fx;
use crate::midi::{
    AudioInputHandle, MidiLearnState, MidiMsg, MidiRecCtx, MidiRecMode, MidiRecorder, MidiService,
    list_audio_inputs,
};
use crate::project::{
    BrowserPrefs, RecentProjects, TemplateKind, config_dir, data_dir, discard_recovery,
    export_zip_package, import_zip_package, load_autosave, load_song_from_path, template_song,
    write_autosave,
};
use crate::song::{
    ArpCfg, ArpDir, AutoClipPoint, AutoPoint, AutoTarget, Channel, ChannelKind, ChordKind,
    ClapPluginCfg, ClipKind, CompCfg, CurveMode, DelayFxCfg, DelaySync, EffectSlot, EqBandType,
    EqCfg, FilterMode, FmAlgo, FmSynthCfg, FxKind, KitPiece, MAX_FX_SLOTS, MapCurve, ModFxCfg,
    ModFxKind, ModTarget, Note, PeakController, QuantizeCfg, RandCfg, ReverbFxCfg, SatCfg, SatKind,
    ScaleKind, SnapGrid, Song, StereoShaperCfg, StretchMode, StrumCfg, SuperSynthCfg,
    TICKS_PER_STEP, TimeSig, WaveKind, default_drum_pads, eval_auto, eval_auto_clip, palette_color,
    tool_chord_ext, tool_duplicate_after, tool_flam, tool_flip, tool_legato, tool_quantize_opts,
    tool_randomize, tool_riff_arp, tool_slice_at, tool_strum_opts,
};
use crate::synth::{apply_preset_to_channel, factory_presets, load_preset_json, save_preset_json};
use eframe::egui;
use egui_dock::{DockState, NodeIndex, TabViewer};
use raw_window_handle::RawWindowHandle;
use std::path::{Path, PathBuf};
use std::time::Instant;

// ---------------------------------------------------------------------------
// UI state (not part of the saved song)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum FocusPanel {
    #[default]
    Rack,
    PianoRoll,
    Playlist,
    Mixer,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DockTab {
    Playlist,
    ChannelRack,
    PianoRoll,
    Mixer,
    MidiAudio,
}

impl DockTab {
    pub fn title(&self) -> &'static str {
        match self {
            DockTab::Playlist => "Playlist (F5)",
            DockTab::ChannelRack => "Channel Rack (F6)",
            DockTab::PianoRoll => "Piano Roll (F7)",
            DockTab::Mixer => "Mixer (F9)",
            DockTab::MidiAudio => "MIDI / Audio",
        }
    }

    pub fn focus_panel(&self) -> FocusPanel {
        match self {
            DockTab::Playlist => FocusPanel::Playlist,
            DockTab::ChannelRack | DockTab::MidiAudio => FocusPanel::Rack,
            DockTab::PianoRoll => FocusPanel::PianoRoll,
            DockTab::Mixer => FocusPanel::Mixer,
        }
    }
}

pub fn default_dock_state() -> DockState<DockTab> {
    let mut dock = DockState::new(vec![
        DockTab::ChannelRack,
        DockTab::PianoRoll,
        DockTab::Playlist,
    ]);
    let tree = dock.main_surface_mut();
    let _ = tree.split_below(NodeIndex::root(), 0.64, vec![DockTab::Mixer]);
    dock
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum BrowserCategory {
    #[default]
    All,
    Drums,
    Bass,
    Fx,
    Presets,
    Plugins,
}

impl BrowserCategory {
    pub const ALL: [BrowserCategory; 6] = [
        BrowserCategory::All,
        BrowserCategory::Drums,
        BrowserCategory::Bass,
        BrowserCategory::Fx,
        BrowserCategory::Presets,
        BrowserCategory::Plugins,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            BrowserCategory::All => "All",
            BrowserCategory::Drums => "Drums",
            BrowserCategory::Bass => "Bass",
            BrowserCategory::Fx => "FX",
            BrowserCategory::Presets => "Presets",
            BrowserCategory::Plugins => "Plugins",
        }
    }
}

#[derive(Clone, Debug)]
pub enum BrowserNode {
    Dir {
        name: String,
        children: Vec<BrowserNode>,
    },
    File {
        name: String,
        path: String,
    },
}

#[derive(Clone, Debug)]
pub enum BrowserDragPayload {
    SamplePath(String),
    PresetIdx(usize),
    FxChoice(usize),
    ClapPlugin(usize),
}

/// FL-style piano roll tools.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum PianoTool {
    #[default]
    Draw,
    Paint,
    Slice,
    Mute,
    Erase,
    Hear,
}

impl PianoTool {
    const ALL: [PianoTool; 6] = [
        PianoTool::Draw,
        PianoTool::Paint,
        PianoTool::Slice,
        PianoTool::Mute,
        PianoTool::Erase,
        PianoTool::Hear,
    ];

    fn name(&self) -> &'static str {
        match self {
            PianoTool::Draw => "Draw",
            PianoTool::Paint => "Paint",
            PianoTool::Slice => "Slice",
            PianoTool::Mute => "Mute",
            PianoTool::Erase => "Erase",
            PianoTool::Hear => "Hear",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum NoteLane {
    #[default]
    Velocity,
    Pan,
    Cutoff,
    FinePitch,
    Release,
}

impl NoteLane {
    const ALL: [NoteLane; 5] = [
        NoteLane::Velocity,
        NoteLane::Pan,
        NoteLane::Cutoff,
        NoteLane::FinePitch,
        NoteLane::Release,
    ];

    fn name(&self) -> &'static str {
        match self {
            NoteLane::Velocity => "Vel",
            NoteLane::Pan => "Pan",
            NoteLane::Cutoff => "Cut",
            NoteLane::FinePitch => "Pitch",
            NoteLane::Release => "Rel",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PianoModalKind {
    Quantize,
    Strum,
    Flam,
    Arp,
    Randomize,
    Flip,
}

struct PianoModal {
    kind: PianoModalKind,
    ch: usize,
    pat: usize,
    backup: Vec<Note>,
    sel: Vec<usize>,
    quant: QuantizeCfg,
    strum: StrumCfg,
    flam_gap: u32,
    flam_vel: f32,
    arp: ArpCfg,
    rand: RandCfg,
    flip_h: bool,
    flip_v: bool,
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

struct NoteDragState {
    anchor_idx: usize,
    kind: DragKind,
    start_tick: i32,
    start_midi: i32,
    orig: Vec<(usize, u32, u8, u32)>,
}

pub struct AppUi {
    pub sel_ch: usize,
    pub sel_pat: usize,
    pub sel_insert: usize,
    pub focus: FocusPanel,
    pub dock_state: DockState<DockTab>,
    pub typing_piano: bool,
    pub typing_octave: i8,
    held_typing_keys: Vec<(egui::Key, u8)>,
    sel_track: usize,
    sel_clip: Option<u64>,
    clip_drag: Option<(u64, ClipDragKind, i32, u32)>,
    playlist_bars_view: u32,
    playlist_snap: u32,
    playlist_paint: PlaylistPaint,
    playlist_auto_drag: Option<(u64, usize)>,
    playlist_auto_pt: Option<usize>,
    clip_sample: usize,
    show_ch_settings: bool,
    sel_pad: usize,
    sel_fx_slot: usize,
    add_fx_kind: usize,
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
    pub show_io_panel: bool,
    // Phase 5: Hardware MIDI, MIDI Learn, Audio Input, Edison-lite.
    pub midi: MidiService,
    pub midi_rec: MidiRecorder,
    pub midi_learn: MidiLearnState,
    pub lock_midi_ch: Option<usize>,
    pub audio_in: Option<AudioInputHandle>,
    pub audio_in_ports: Vec<String>,
    pub selected_audio_in: Option<usize>,
    pub audio_in_ch: usize,
    pub audio_in_stereo: bool,
    pub edison_sample: Option<usize>,
    pub edison_sel: (f32, f32),
    pub edison_zoom: f32,
    pub edison_slices: Vec<usize>,
    tool: PianoTool,
    piano_bot: u8,
    piano_zoom_x: f32,
    piano_zoom_y: f32,
    snap_grid: SnapGrid,
    scale: ScaleKind,
    root: u8,
    snap_scale: bool,
    chord: ChordKind,
    chord_inv: u8,
    next_slide: bool,
    show_ghosts: bool,
    editable_ghosts: bool,
    note_lane: NoteLane,
    sel_notes: Vec<usize>,
    marquee: Option<(egui::Pos2, egui::Pos2)>,
    prop_note: Option<usize>,
    drag: Option<NoteDragState>,
    press_cell: Option<(i32, i32)>,
    paint_last_tick: Option<u32>,
    modal: Option<PianoModal>,
    auto_sel: AutoSel,
    auto_drag: Option<usize>,
    pub save_path: Option<PathBuf>,
    pub recent_projects: RecentProjects,
    pub browser_prefs: BrowserPrefs,
    browser_dir: String,
    browser_search: String,
    browser_category: BrowserCategory,
    browser_fav_only: bool,
    browser_sync_preview: bool,
    browser_tree: Vec<BrowserNode>,
    browser_tree_dirty: bool,
    pub show_export_modal: bool,
    pub export_cfg: ExportConfig,
    pub export_target_path: String,
    pub show_recovery_prompt: bool,
    last_autosave: Instant,
    dirty_since_autosave: bool,
    note_clipboard: Vec<Note>,
    undo: Vec<Song>,
    redo: Vec<Song>,
    history_current: Option<Song>,
    history_pending: bool,
    pub status: String,
    taps: Vec<Instant>,
    show_tutorial: bool,
    pub clap: ClapManager,
    pub clap_catalog: ClapCatalog,
    pub clap_param_filter: String,
    pub parent_window: Option<RawWindowHandle>,
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
        let cfg = config_dir();
        let recent_projects = cfg
            .as_deref()
            .map(RecentProjects::load_from)
            .unwrap_or_default();
        let browser_prefs = cfg
            .as_deref()
            .map(BrowserPrefs::load_from)
            .unwrap_or_default();
        Self {
            sel_ch: 4,
            sel_pat: 0,
            sel_insert: 1,
            focus: FocusPanel::Rack,
            dock_state: default_dock_state(),
            typing_piano: false,
            typing_octave: 4,
            held_typing_keys: Vec::new(),
            sel_track: 0,
            sel_clip: None,
            clip_drag: None,
            playlist_bars_view: 8,
            playlist_snap: 16,
            playlist_paint: PlaylistPaint::Pattern,
            playlist_auto_drag: None,
            playlist_auto_pt: None,
            clip_sample: 0,
            show_ch_settings: false,
            sel_pad: 0,
            sel_fx_slot: 0,
            add_fx_kind: 0,
            step_graph: StepGraphMode::Off,
            route_target: 0,
            route_amount: 1.0,
            route_sidechain: false,
            show_browser: true,
            show_rack: true,
            show_piano: true,
            show_playlist: true,
            show_mixer: true,
            show_io_panel: false,
            midi: MidiService::default(),
            midi_rec: MidiRecorder::default(),
            midi_learn: MidiLearnState::default(),
            lock_midi_ch: None,
            audio_in: None,
            audio_in_ports: Vec::new(),
            selected_audio_in: None,
            audio_in_ch: 0,
            audio_in_stereo: true,
            edison_sample: None,
            edison_sel: (0.0, 1.0),
            edison_zoom: 1.0,
            edison_slices: Vec::new(),
            tool: PianoTool::Draw,
            piano_bot: 40,
            piano_zoom_x: 1.0,
            piano_zoom_y: 1.0,
            snap_grid: SnapGrid::S1_16,
            scale: ScaleKind::Major,
            root: 0,
            snap_scale: false,
            chord: ChordKind::Off,
            chord_inv: 0,
            next_slide: false,
            show_ghosts: true,
            editable_ghosts: false,
            note_lane: NoteLane::Velocity,
            sel_notes: Vec::new(),
            marquee: None,
            prop_note: None,
            drag: None,
            press_cell: None,
            paint_last_tick: None,
            modal: None,
            auto_sel: AutoSel::Vol,
            auto_drag: None,
            save_path: None,
            recent_projects,
            browser_prefs,
            browser_dir: home_music_dir(),
            browser_search: String::new(),
            browser_category: BrowserCategory::All,
            browser_fav_only: false,
            browser_sync_preview: false,
            browser_tree: Vec::new(),
            browser_tree_dirty: true,
            show_export_modal: false,
            export_cfg: ExportConfig::default(),
            export_target_path: "mixdown.wav".into(),
            show_recovery_prompt: false,
            last_autosave: Instant::now(),
            dirty_since_autosave: false,
            note_clipboard: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            history_current: None,
            history_pending: false,
            status: "press Play or open the Tutorial".into(),
            taps: Vec::new(),
            show_tutorial: !tutorial_seen(),
            clap: ClapManager::default(),
            clap_catalog: load_cached_catalog(),
            clap_param_filter: String::new(),
            parent_window: None,
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
            self.dirty_since_autosave = true;
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

    pub fn maybe_autosave(&mut self, song: &Song) {
        if !self.dirty_since_autosave
            || self.last_autosave.elapsed() < std::time::Duration::from_secs(30)
        {
            return;
        }
        if let Some(dir) = data_dir()
            && write_autosave(&dir, song).is_ok()
        {
            self.dirty_since_autosave = false;
            self.last_autosave = Instant::now();
        }
    }

    pub fn reset_history(&mut self, song: &Song) {
        self.undo.clear();
        self.redo.clear();
        self.history_current = Some(song.clone());
        self.history_pending = false;
        self.dirty_since_autosave = false;
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

    pub fn rec_snap_ticks(&self) -> u32 {
        self.snap_grid.step_ticks(12.0).round().max(1.0) as u32
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
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let notes = s
        .song
        .patterns
        .get(ui.sel_pat)
        .and_then(|pattern| pattern.notes.get(ch));
    let Some(notes) = notes.filter(|notes| !notes.is_empty()) else {
        ui.status = "no notes to copy".into();
        return false;
    };
    if ui.sel_notes.is_empty() {
        ui.note_clipboard = notes.clone();
    } else {
        ui.note_clipboard = ui
            .sel_notes
            .iter()
            .filter_map(|&idx| notes.get(idx).copied())
            .collect();
    }
    if ui.note_clipboard.is_empty() {
        ui.note_clipboard = notes.clone();
    }
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
    ui.sel_notes.clear();
    ui.status = format!("pasted {} notes", ui.note_clipboard.len());
    true
}

pub fn select_all_notes(ui: &mut AppUi, s: &AppState) {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let len = s
        .song
        .patterns
        .get(ui.sel_pat)
        .and_then(|p| p.notes.get(ch))
        .map(|ns| ns.len())
        .unwrap_or(0);
    ui.sel_notes = (0..len).collect();
    ui.status = format!("selected {len} notes");
}

pub fn invert_note_selection(ui: &mut AppUi, s: &AppState) {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let len = s
        .song
        .patterns
        .get(ui.sel_pat)
        .and_then(|p| p.notes.get(ch))
        .map(|ns| ns.len())
        .unwrap_or(0);
    ui.sel_notes = (0..len).filter(|i| !ui.sel_notes.contains(i)).collect();
    ui.status = format!("selected {} notes", ui.sel_notes.len());
}

pub fn duplicate_selected_notes(ui: &mut AppUi, s: &mut AppState) -> bool {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let Some(pat) = s.song.patterns.get_mut(ui.sel_pat) else {
        return false;
    };
    let max_ticks = pat.len_ticks();
    let Some(notes) = pat.notes.get_mut(ch) else {
        return false;
    };
    let added = tool_duplicate_after(notes, &ui.sel_notes, max_ticks);
    if added.is_empty() {
        return false;
    }
    ui.sel_notes = added;
    ui.status = format!("duplicated {} notes", ui.sel_notes.len());
    true
}

pub fn legato_selected_notes(ui: &mut AppUi, s: &mut AppState) -> bool {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let Some(pat) = s.song.patterns.get_mut(ui.sel_pat) else {
        return false;
    };
    let max_ticks = pat.len_ticks();
    let Some(notes) = pat.notes.get_mut(ch) else {
        return false;
    };
    if notes.is_empty() {
        return false;
    }
    tool_legato(notes, &ui.sel_notes, max_ticks);
    ui.status = "legato applied".into();
    true
}

pub fn delete_selected_notes(ui: &mut AppUi, s: &mut AppState) -> bool {
    if ui.sel_notes.is_empty() {
        return false;
    }
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let Some(notes) = s
        .song
        .patterns
        .get_mut(ui.sel_pat)
        .and_then(|p| p.notes.get_mut(ch))
    else {
        return false;
    };
    let mut idxs = std::mem::take(&mut ui.sel_notes);
    idxs.sort_unstable();
    idxs.dedup();
    let mut removed = false;
    for idx in idxs.into_iter().rev() {
        if idx < notes.len() {
            notes.remove(idx);
            removed = true;
        }
    }
    ui.prop_note = None;
    removed
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
// Workspace docking, typing piano, shortcuts, and project helpers
// ---------------------------------------------------------------------------

fn sync_one_tab(dock: &mut DockState<DockTab>, tab: DockTab, show: bool) {
    let existing = dock.find_tab(&tab);
    match (show, existing) {
        (true, None) => dock.push_to_first_leaf(tab),
        (false, Some(path)) => {
            dock.remove_tab(path);
        }
        _ => {}
    }
}

pub fn focus_dock_tab(dock: &mut DockState<DockTab>, tab: DockTab) {
    if let Some(loc) = dock.find_tab(&tab) {
        dock.set_focused_node_and_surface(loc.node_path());
        let _ = dock.set_active_tab(loc);
    }
}

pub fn sync_dock_tabs(ui: &mut AppUi) {
    sync_one_tab(&mut ui.dock_state, DockTab::Playlist, ui.show_playlist);
    sync_one_tab(&mut ui.dock_state, DockTab::ChannelRack, ui.show_rack);
    sync_one_tab(&mut ui.dock_state, DockTab::PianoRoll, ui.show_piano);
    sync_one_tab(&mut ui.dock_state, DockTab::Mixer, ui.show_mixer);
    sync_one_tab(&mut ui.dock_state, DockTab::MidiAudio, ui.show_io_panel);
}

pub fn show_dock_area(ui: &mut AppUi, state: &mut AppState, u: &mut egui::Ui) {
    let mut dock = std::mem::replace(&mut ui.dock_state, DockState::new(vec![]));
    let style = crate::ui_widgets::dock_style(u.style());
    egui_dock::DockArea::new(&mut dock)
        .style(style)
        .show_inside(u, &mut ThunderTabViewer { ui, state });
    ui.dock_state = dock;
    sync_dock_tabs(ui);
}

struct ThunderTabViewer<'a> {
    ui: &'a mut AppUi,
    state: &'a mut AppState,
}

impl TabViewer for ThunderTabViewer<'_> {
    type Tab = DockTab;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(*tab)
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        tab.title().into()
    }

    fn ui(&mut self, u: &mut egui::Ui, tab: &mut Self::Tab) {
        let pressed = u.ctx().input(|i| i.pointer.primary_pressed());
        let rect = u.available_rect_before_wrap();
        if pressed
            && u.ctx()
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|p| rect.contains(p))
        {
            self.ui.focus = tab.focus_panel();
        }
        if *tab == DockTab::PianoRoll {
            piano_roll(self.ui, self.state, u);
        } else {
            egui::ScrollArea::vertical()
                .id_salt(("dock_tab_scroll", *tab))
                .show(u, |u| match tab {
                    DockTab::Playlist => playlist(self.ui, self.state, u),
                    DockTab::ChannelRack => channel_rack(self.ui, self.state, u),
                    DockTab::PianoRoll => {}
                    DockTab::Mixer => mixer(self.ui, self.state, u),
                    DockTab::MidiAudio => io_and_controllers_panel(self.ui, self.state, u),
                });
        }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> egui_dock::tab_viewer::OnCloseResponse {
        match tab {
            DockTab::Playlist => self.ui.show_playlist = false,
            DockTab::ChannelRack => self.ui.show_rack = false,
            DockTab::PianoRoll => self.ui.show_piano = false,
            DockTab::Mixer => self.ui.show_mixer = false,
            DockTab::MidiAudio => self.ui.show_io_panel = false,
        }
        egui_dock::tab_viewer::OnCloseResponse::Close
    }
}

/// FL Studio QWERTY two-octave keyboard mapping relative to `octave` (default C4 = 60).
pub fn typing_key_to_midi(key: egui::Key, octave: i8) -> Option<u8> {
    let offset: i16 = match key {
        egui::Key::Z => 0,
        egui::Key::S => 1,
        egui::Key::X => 2,
        egui::Key::D => 3,
        egui::Key::C => 4,
        egui::Key::V => 5,
        egui::Key::G => 6,
        egui::Key::B => 7,
        egui::Key::H => 8,
        egui::Key::N => 9,
        egui::Key::J => 10,
        egui::Key::M => 11,
        egui::Key::Q => 12,
        egui::Key::Num2 => 13,
        egui::Key::W => 14,
        egui::Key::Num3 => 15,
        egui::Key::E => 16,
        egui::Key::R => 17,
        egui::Key::Num5 => 18,
        egui::Key::T => 19,
        egui::Key::Num6 => 20,
        egui::Key::Y => 21,
        egui::Key::Num7 => 22,
        egui::Key::U => 23,
        egui::Key::I => 24,
        egui::Key::Num9 => 25,
        egui::Key::O => 26,
        egui::Key::Num0 => 27,
        egui::Key::P => 28,
        _ => return None,
    };
    let base = i16::from(octave.clamp(1, 7) + 1) * 12;
    let note = base + offset;
    (0..=127).contains(&note).then_some(note as u8)
}

const TYPING_PIANO_KEYS: [egui::Key; 29] = [
    egui::Key::Z,
    egui::Key::S,
    egui::Key::X,
    egui::Key::D,
    egui::Key::C,
    egui::Key::V,
    egui::Key::G,
    egui::Key::B,
    egui::Key::H,
    egui::Key::N,
    egui::Key::J,
    egui::Key::M,
    egui::Key::Q,
    egui::Key::Num2,
    egui::Key::W,
    egui::Key::Num3,
    egui::Key::E,
    egui::Key::R,
    egui::Key::Num5,
    egui::Key::T,
    egui::Key::Num6,
    egui::Key::Y,
    egui::Key::Num7,
    egui::Key::U,
    egui::Key::I,
    egui::Key::Num9,
    egui::Key::O,
    egui::Key::Num0,
    egui::Key::P,
];

fn release_typing_keys(ui: &mut AppUi, s: &mut AppState) {
    let target_ch = ui
        .lock_midi_ch
        .unwrap_or(ui.sel_ch)
        .min(s.song.channels.len().saturating_sub(1));
    let snap_ticks = ui.rec_snap_ticks();
    for &(_, note) in &ui.held_typing_keys {
        let msg = MidiMsg::NoteOff { ch: 0, note };
        s.send_midi_msg(msg);
        ui.midi_rec.handle_msg(
            &mut s.song,
            msg,
            MidiRecCtx {
                pat: ui.sel_pat,
                ch: target_ch,
                cur_tick: s.rt.cur_tick,
                playing: s.rt.playing,
                snap_grid: snap_ticks,
            },
        );
    }
    ui.held_typing_keys.clear();
}

pub fn process_typing_piano(ui: &mut AppUi, s: &mut AppState, ctx: &egui::Context) {
    if !ui.typing_piano || ctx.egui_wants_keyboard_input() {
        if !ui.held_typing_keys.is_empty() {
            release_typing_keys(ui, s);
        }
        return;
    }
    let has_mod = ctx.input(|i| i.modifiers.command || i.modifiers.ctrl || i.modifiers.alt);
    let target_ch = ui
        .lock_midi_ch
        .unwrap_or(ui.sel_ch)
        .min(s.song.channels.len().saturating_sub(1));
    let snap_ticks = ui.rec_snap_ticks();

    if !has_mod {
        if ctx.input(|i| i.key_pressed(egui::Key::OpenBracket) || i.key_pressed(egui::Key::Minus)) {
            ui.typing_octave = (ui.typing_octave - 1).max(1);
            ui.status = format!("typing octave C{}", ui.typing_octave);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::CloseBracket) || i.key_pressed(egui::Key::Equals))
        {
            ui.typing_octave = (ui.typing_octave + 1).min(7);
            ui.status = format!("typing octave C{}", ui.typing_octave);
        }
    }

    for key in TYPING_PIANO_KEYS {
        let pressed = ctx.input(|i| i.key_pressed(key));
        let released = ctx.input(|i| i.key_released(key));
        if pressed
            && !has_mod
            && !ui.held_typing_keys.iter().any(|&(k, _)| k == key)
            && let Some(midi) = typing_key_to_midi(key, ui.typing_octave)
        {
            ui.held_typing_keys.push((key, midi));
            let msg = MidiMsg::NoteOn {
                ch: 0,
                note: midi,
                vel: 0.85,
            };
            s.send_midi_msg(msg);
            ui.midi_rec.handle_msg(
                &mut s.song,
                msg,
                MidiRecCtx {
                    pat: ui.sel_pat,
                    ch: target_ch,
                    cur_tick: s.rt.cur_tick,
                    playing: s.rt.playing,
                    snap_grid: snap_ticks,
                },
            );
        }
        if released && let Some(pos) = ui.held_typing_keys.iter().position(|&(k, _)| k == key) {
            let (_, midi) = ui.held_typing_keys.remove(pos);
            let msg = MidiMsg::NoteOff { ch: 0, note: midi };
            s.send_midi_msg(msg);
            ui.midi_rec.handle_msg(
                &mut s.song,
                msg,
                MidiRecCtx {
                    pat: ui.sel_pat,
                    ch: target_ch,
                    cur_tick: s.rt.cur_tick,
                    playing: s.rt.playing,
                    snap_grid: snap_ticks,
                },
            );
        }
    }
}

pub fn handle_workspace_shortcuts(ui: &mut AppUi, s: &mut AppState, ctx: &egui::Context) {
    if ctx.egui_wants_keyboard_input() {
        return;
    }
    let (space, cmd, shift, z, y, c, v, l, a, i_k, b, t, s_k, del, f5, f6, f7, f8, f9) =
        ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.modifiers.command || i.modifiers.ctrl,
                i.modifiers.shift,
                i.key_pressed(egui::Key::Z),
                i.key_pressed(egui::Key::Y),
                i.key_pressed(egui::Key::C),
                i.key_pressed(egui::Key::V),
                i.key_pressed(egui::Key::L),
                i.key_pressed(egui::Key::A),
                i.key_pressed(egui::Key::I),
                i.key_pressed(egui::Key::B),
                i.key_pressed(egui::Key::T),
                i.key_pressed(egui::Key::S),
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::F5),
                i.key_pressed(egui::Key::F6),
                i.key_pressed(egui::Key::F7),
                i.key_pressed(egui::Key::F8),
                i.key_pressed(egui::Key::F9),
            )
        });

    if space {
        let next = !s.rt.playing;
        s.set_playing(next);
    }
    if f5 {
        ui.show_playlist = !ui.show_playlist;
        if ui.show_playlist {
            ui.focus = FocusPanel::Playlist;
        }
    }
    if f6 {
        ui.show_rack = !ui.show_rack;
        if ui.show_rack {
            ui.focus = FocusPanel::Rack;
        }
    }
    if f7 {
        ui.show_piano = !ui.show_piano;
        if ui.show_piano {
            ui.focus = FocusPanel::PianoRoll;
        }
    }
    if f8 {
        ui.show_browser = !ui.show_browser;
    }
    if f9 {
        ui.show_mixer = !ui.show_mixer;
        if ui.show_mixer {
            ui.focus = FocusPanel::Mixer;
        }
    }

    if cmd && t {
        ui.typing_piano = !ui.typing_piano;
        if !ui.typing_piano {
            release_typing_keys(ui, s);
        }
        ui.status = if ui.typing_piano {
            format!("typing keyboard piano ON (C{})", ui.typing_octave)
        } else {
            "typing keyboard piano OFF".into()
        };
    } else if cmd && s_k {
        save_current_project(ui, s);
    } else if cmd && z && shift {
        redo(ui, s);
    } else if cmd && z {
        undo(ui, s);
    } else if cmd && y {
        redo(ui, s);
    } else if cmd && c {
        copy_notes(ui, s);
    } else if cmd && v {
        paste_notes(ui, s);
    } else if cmd && a && ui.focus == FocusPanel::PianoRoll {
        select_all_notes(ui, s);
    } else if cmd && i_k && ui.focus == FocusPanel::PianoRoll {
        invert_note_selection(ui, s);
    } else if cmd && b && ui.focus == FocusPanel::PianoRoll {
        duplicate_selected_notes(ui, s);
    } else if cmd && l {
        if ui.focus == FocusPanel::PianoRoll {
            legato_selected_notes(ui, s);
        } else {
            link_selected_channel_to_insert(ui, s);
        }
    } else if del {
        if ui.focus == FocusPanel::PianoRoll {
            if delete_selected_notes(ui, s) {
                ui.status = "deleted selected notes".into();
            }
        } else if ui.focus == FocusPanel::Playlist
            && let Some(cid) = ui.sel_clip.take()
            && s.song.remove_clip(cid)
        {
            ui.status = "deleted clip".into();
        }
    }
    sync_dock_tabs(ui);
}

fn apply_loaded_song_to_ui(ui: &mut AppUi, s: &mut AppState, song: Song, path: Option<PathBuf>) {
    ui.sel_ch = ui.sel_ch.min(song.channels.len().saturating_sub(1));
    ui.sel_pat = ui.sel_pat.min(song.patterns.len().saturating_sub(1));
    ui.sel_insert = ui.sel_insert.min(song.mixer_tracks.len().saturating_sub(1));
    s.set_inspect_insert(ui.sel_insert);
    s.apply_loaded_song(song);
    s.set_pattern(ui.sel_pat);
    ui.reset_history(&s.song);
    ui.save_path = path.clone();
    if let Some(p) = path {
        ui.recent_projects.push(&p.to_string_lossy());
        if let Some(cfg) = config_dir() {
            ui.recent_projects.save_to(&cfg);
        }
    }
}

fn save_project_to_path(ui: &mut AppUi, s: &AppState, path: &Path) {
    let pstr = path.to_string_lossy().into_owned();
    match s.song.save(&pstr) {
        Ok(()) => {
            ui.save_path = Some(path.to_path_buf());
            ui.dirty_since_autosave = false;
            ui.recent_projects.push(&pstr);
            if let Some(cfg) = config_dir() {
                ui.recent_projects.save_to(&cfg);
            }
            ui.status = format!("saved {pstr}");
        }
        Err(e) => ui.status = format!("save failed: {e}"),
    }
}

fn save_current_project(ui: &mut AppUi, s: &AppState) {
    if let Some(p) = ui.save_path.clone() {
        save_project_to_path(ui, s, &p);
    } else if let Some(p) = rfd::FileDialog::new()
        .add_filter("Thunder Project", &["json"])
        .set_file_name("song.json")
        .save_file()
    {
        save_project_to_path(ui, s, &p);
    }
}

fn open_project_file(ui: &mut AppUi, s: &mut AppState, path: &Path) {
    match load_song_from_path(path) {
        Ok((song, missing)) => {
            let pstr = path.to_string_lossy().into_owned();
            apply_loaded_song_to_ui(ui, s, song, Some(path.to_path_buf()));
            ui.status = if missing == 0 {
                format!("loaded {pstr}")
            } else {
                format!("loaded {pstr} ({missing} samples missing)")
            };
        }
        Err(e) => ui.status = format!("load failed: {e}"),
    }
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

enum ParamCtxAction {
    CreateAuto(AutoTarget),
    MidiLearn(AutoTarget),
}

fn collect_param_ctx(resp: &egui::Response, target: AutoTarget, out: &mut Option<ParamCtxAction>) {
    resp.context_menu(|u| {
        if u.button("Create automation clip").clicked() {
            *out = Some(ParamCtxAction::CreateAuto(target));
            u.close();
        }
        if u.button("Link to controller (MIDI Learn)").clicked() {
            *out = Some(ParamCtxAction::MidiLearn(target));
            u.close();
        }
    });
}

fn apply_param_ctx(ui: &mut AppUi, s: &mut AppState, act: ParamCtxAction) {
    match act {
        ParamCtxAction::CreateAuto(target) => {
            let len = s.song.time_sig.ticks_per_bar().max(TICKS_PER_STEP * 4);
            let track = ui
                .sel_track
                .min(s.song.playlist_tracks.len().saturating_sub(1));
            let cid = s.song.add_auto_clip(track, 0, len, target);
            ui.sel_clip = Some(cid);
            ui.show_playlist = true;
            sync_dock_tabs(ui);
            ui.status = format!("created automation clip: {}", target.label());
        }
        ParamCtxAction::MidiLearn(target) => {
            ui.midi_learn.armed_target = Some(target);
            ui.show_io_panel = true;
            sync_dock_tabs(ui);
            ui.status = format!("MIDI Learn armed for {}", target.label());
        }
    }
}

fn param_context_menu(resp: &egui::Response, ui: &mut AppUi, s: &mut AppState, target: AutoTarget) {
    let mut act = None;
    collect_param_ctx(resp, target, &mut act);
    if let Some(a) = act {
        apply_param_ctx(ui, s, a);
    }
}

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

    u.horizontal_wrapped(|u| {
        u.menu_button("File", |u| {
            if u.button("New Empty").clicked() {
                apply_loaded_song_to_ui(ui, s, crate::song::new_empty(), None);
                ui.status = "new empty project".into();
                u.close();
            }
            u.menu_button("New from Template", |u| {
                for tpl in TemplateKind::ALL {
                    if u.button(tpl.name()).clicked() {
                        apply_loaded_song_to_ui(ui, s, template_song(tpl), None);
                        ui.status = format!("created {}", tpl.name());
                        u.close();
                    }
                }
            });
            u.separator();
            if u.button("Open Project...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Thunder / ZIP", &["json", "zip"])
                    .pick_file()
                {
                    open_project_file(ui, s, &p);
                }
                u.close();
            }
            u.menu_button("Recent Projects", |u| {
                let paths = ui.recent_projects.paths.clone();
                if paths.is_empty() {
                    u.label("(none)");
                }
                for pstr in paths {
                    let lbl = Path::new(&pstr)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&pstr);
                    if u.button(lbl).on_hover_text(&pstr).clicked() {
                        open_project_file(ui, s, Path::new(&pstr));
                        u.close();
                    }
                }
            });
            if u.button("Save (Ctrl+S)").clicked() {
                save_current_project(ui, s);
                u.close();
            }
            if u.button("Save As...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Thunder Project", &["json"])
                    .set_file_name("song.json")
                    .save_file()
                {
                    save_project_to_path(ui, s, &p);
                }
                u.close();
            }
            u.separator();
            if u.button("Export ZIP Package...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Zipped Loop Package", &["zip"])
                    .set_file_name("project_bundle.zip")
                    .save_file()
                {
                    match export_zip_package(&s.song, &p) {
                        Ok(m) => ui.status = m,
                        Err(e) => ui.status = format!("zip export failed: {e}"),
                    }
                }
                u.close();
            }
            if u.button("Import ZIP Package...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Zipped Loop Package", &["zip"])
                    .pick_file()
                {
                    let work = data_dir()
                        .unwrap_or_else(std::env::temp_dir)
                        .join("zip_work");
                    match import_zip_package(&p, &work) {
                        Ok(song) => {
                            apply_loaded_song_to_ui(ui, s, song, None);
                            ui.status = format!("imported {}", p.display());
                        }
                        Err(e) => ui.status = format!("zip import failed: {e}"),
                    }
                }
                u.close();
            }
            u.separator();
            if u.button("Export Audio...").clicked() {
                ui.show_export_modal = true;
                u.close();
            }
            if u.button("Import MIDI...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Standard MIDI", &["mid", "midi"])
                    .pick_file()
                {
                    let pat = ui.sel_pat;
                    match crate::export::import_midi_into_song(
                        &mut s.song,
                        pat,
                        &p.to_string_lossy(),
                    ) {
                        Ok(n) => {
                            s.rt.ensure_channels(&s.song);
                            ui.status = format!("imported {n} MIDI notes");
                        }
                        Err(e) => ui.status = format!("midi import failed: {e}"),
                    }
                }
                u.close();
            }
            if u.button("Export MIDI...").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Standard MIDI", &["mid", "midi"])
                    .set_file_name("song.mid")
                    .save_file()
                {
                    match crate::export::export_midi(&s.song, ui.sel_pat, &p.to_string_lossy()) {
                        Ok(m) => ui.status = m,
                        Err(e) => ui.status = format!("midi export failed: {e}"),
                    }
                }
                u.close();
            }
            if u.button("Save Take WAV...").clicked() {
                if !s.has_take() {
                    ui.status = "nothing recorded yet".into();
                } else if let Some(p) = rfd::FileDialog::new()
                    .add_filter("WAV Audio", &["wav"])
                    .set_file_name("take.wav")
                    .save_file()
                {
                    match s.save_take(&p.to_string_lossy()) {
                        Ok(n) => ui.status = format!("saved take ({n} samples)"),
                        Err(e) => ui.status = format!("take save failed: {e}"),
                    }
                }
                u.close();
            }
        });
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
            sync_dock_tabs(ui);
        }
        let mut v = ui.show_piano;
        if u.toggle_value(&mut v, "Piano")
            .on_hover_text("Piano roll (F7)")
            .changed()
        {
            ui.show_piano = v;
            sync_dock_tabs(ui);
        }
        let mut v = ui.show_playlist;
        if u.toggle_value(&mut v, "List")
            .on_hover_text("Playlist (F5)")
            .changed()
        {
            ui.show_playlist = v;
            sync_dock_tabs(ui);
        }
        let mut v = ui.show_mixer;
        if u.toggle_value(&mut v, "Mix")
            .on_hover_text("Mixer (F9)")
            .changed()
        {
            ui.show_mixer = v;
            sync_dock_tabs(ui);
        }
        if u.toggle_value(&mut ui.show_io_panel, "MIDI/Audio")
            .on_hover_text("Hardware MIDI, MIDI Learn, Audio Input & Peak Controllers")
            .changed()
        {
            sync_dock_tabs(ui);
        }
        if u.toggle_value(&mut ui.typing_piano, "Keys")
            .on_hover_text("Typing Keyboard to Piano (Ctrl+T)")
            .changed()
            && !ui.typing_piano
        {
            release_typing_keys(ui, s);
        }
        if ui.typing_piano {
            let mut oct = ui.typing_octave;
            u.add(egui::DragValue::new(&mut oct).range(1..=7).prefix("Oct "));
            ui.typing_octave = oct;
        }
        if u.button("Layout")
            .on_hover_text("Reset docked panels to default FL layout")
            .clicked()
        {
            ui.show_rack = true;
            ui.show_piano = true;
            ui.show_playlist = true;
            ui.show_mixer = true;
            ui.dock_state = default_dock_state();
            sync_dock_tabs(ui);
        }
        u.separator();
        let playing = s.rt.playing;
        if u.button(if playing { "⏹ Stop" } else { "▶ Play" })
            .on_hover_text("Spacebar works too")
            .clicked()
        {
            let next = !playing;
            s.set_playing(next);
            if next {
                if s.rt.song_mode && s.song.clips.is_empty() {
                    ui.status =
                        "Song mode has no Playlist clips: add clips or switch to Pat".into();
                } else if !s.rt.song_mode
                    && s.song
                        .patterns
                        .get(ui.sel_pat)
                        .is_none_or(|p| p.notes.iter().all(|ns| ns.is_empty()))
                {
                    ui.status =
                        "Pattern is empty: click steps in Channel Rack or Load Demo in Tutorial"
                            .into();
                }
            }
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
        if u.toggle_value(&mut ui.midi_rec.armed, "MIDI Rec")
            .on_hover_text("Record incoming MIDI notes into Piano Roll")
            .changed()
        {
            ui.midi_rec.reset_pass();
        }
        let take_secs = s.take_duration_secs();
        u.colored_label(
            if s.rt.recording {
                crate::ui_widgets::ACCENT_RED
            } else {
                crate::ui_widgets::TEXT_MUTED
            },
            format!(
                "{}take {}:{:02}{}",
                if s.rt.recording { "● " } else { "" },
                take_secs / 60,
                take_secs % 60,
                if s.rt.take_full { " (full)" } else { "" }
            ),
        );
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
        u.add(
            egui::DragValue::new(&mut bpm)
                .range(60.0..=200.0)
                .speed(0.4)
                .suffix(" BPM"),
        );
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
        if crate::ui_widgets::daw_knob(
            u,
            &mut swing,
            0.0..=0.6,
            0.0,
            "Swg",
            crate::ui_widgets::ACCENT_AMBER,
        )
        .on_hover_text(format!(
            "Swing: {:.0}% (double-click to reset)",
            swing * 100.0
        ))
        .changed()
        {
            s.song.swing = swing;
        }
        if crate::ui_widgets::led_toggle(
            u,
            s.song.metronome,
            "Click",
            crate::ui_widgets::ACCENT_AMBER,
            egui::vec2(42.0, 22.0),
        )
        .on_hover_text("Toggle metronome click")
        .clicked()
        {
            s.song.metronome = !s.song.metronome;
        }
        if crate::ui_widgets::led_toggle(
            u,
            s.song.loop_song,
            "Loop",
            crate::ui_widgets::ACCENT_GREEN,
            egui::vec2(40.0, 22.0),
        )
        .on_hover_text("Toggle song loop")
        .clicked()
        {
            s.song.loop_song = !s.song.loop_song;
        }
        let mut master = s.song.master_vol;
        let m_resp = u.add(
            egui::Slider::new(&mut master, 0.0..=1.5)
                .text("Master")
                .fixed_decimals(2),
        );
        s.song.master_vol = master;
        param_context_menu(&m_resp, ui, s, AutoTarget::MasterVol);
        // Position readout.
        let tick = s.rt.tick_pos.max(0.0) as u64;
        let bar_ticks = s.song.time_sig.ticks_per_bar().max(1) as u64;
        let beat_ticks = s.song.time_sig.ticks_per_beat().max(1) as u64;
        u.label(
            egui::RichText::new(format!(
                "{:02}:{:01}:{:02}",
                tick / bar_ticks + 1,
                (tick % bar_ticks) / beat_ticks + 1,
                tick % beat_ticks
            ))
            .monospace()
            .strong()
            .color(crate::ui_widgets::ACCENT_GREEN),
        );
        crate::ui_widgets::draw_peak_meter_h(u, s.rt.peak, 74.0, 16.0);
        if s.rt.xruns > 0 {
            u.colored_label(
                crate::ui_widgets::ACCENT_AMBER,
                format!("xruns:{}", s.rt.xruns),
            );
        }
    });
    u.horizontal_wrapped(|u| {
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
        egui::ComboBox::from_id_salt("midi_rec_mode")
            .width(68.0)
            .selected_text(ui.midi_rec.mode.name())
            .show_ui(u, |u| {
                for m in MidiRecMode::ALL {
                    u.selectable_value(&mut ui.midi_rec.mode, m, m.name());
                }
            });
        u.checkbox(&mut s.song.auto_quantize_rec, "Auto-Q");
        let ci_lbl = match s.song.count_in_bars {
            0 => "Count: Off",
            1 => "Count: 1b",
            _ => "Count: 2b",
        };
        egui::ComboBox::from_id_salt("count_in_sel")
            .width(74.0)
            .selected_text(ci_lbl)
            .show_ui(u, |u| {
                u.selectable_value(&mut s.song.count_in_bars, 0, "Off");
                u.selectable_value(&mut s.song.count_in_bars, 1, "1 Bar");
                u.selectable_value(&mut s.song.count_in_bars, 2, "2 Bars");
            });
        u.separator();
        let proj_lbl = ui
            .save_path
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("Unsaved");
        u.label(format!("Project: {proj_lbl}"));
        if u.button("Export...").clicked() {
            ui.show_export_modal = true;
        }
        if s.has_take()
            && u.button("Save Take...")
                .on_hover_text("Save recorded take as WAV via native dialog")
                .clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("WAV Audio", &["wav"])
                .set_file_name("take.wav")
                .save_file()
        {
            match s.save_take(&p.to_string_lossy()) {
                Ok(n) => ui.status = format!("saved take ({n} samples)"),
                Err(e) => ui.status = format!("take save failed: {e}"),
            }
        }
        if u.button("→ Clip")
            .on_hover_text("Commit recorded take into Playlist audio clip")
            .clicked()
        {
            match s.commit_audio_take(ui.sel_track, 0) {
                Ok(cid) => {
                    ui.sel_clip = Some(cid);
                    ui.show_playlist = true;
                    sync_dock_tabs(ui);
                    ui.status = "committed audio take to Playlist".into();
                }
                Err(e) => ui.status = format!("take commit failed: {e}"),
            }
        }
    });
}

pub fn io_and_controllers_panel(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.group(|u| {
        u.horizontal_wrapped(|u| {
            u.strong("Hardware MIDI & MIDI Learn");
            if u.button("↻ Refresh Devices").clicked() {
                ui.midi.refresh();
                ui.audio_in_ports = list_audio_inputs();
                ui.status = format!(
                    "found {} MIDI in, {} Audio in",
                    ui.midi.in_ports.len(),
                    ui.audio_in_ports.len()
                );
            }
            let in_lbl = ui
                .midi
                .selected_in
                .and_then(|i| ui.midi.in_ports.get(i))
                .cloned()
                .unwrap_or_else(|| "(none)".into());
            let mut req_in = ui.midi.selected_in;
            egui::ComboBox::from_id_salt("midi_in_sel")
                .selected_text(format!("In: {in_lbl}"))
                .show_ui(u, |u| {
                    u.selectable_value(&mut req_in, None, "(none)");
                    for (i, name) in ui.midi.in_ports.iter().enumerate() {
                        u.selectable_value(&mut req_in, Some(i), name.clone());
                    }
                });
            if req_in != ui.midi.selected_in {
                match req_in {
                    None => {
                        ui.midi.disconnect_in();
                        ui.status = "MIDI input disconnected".into();
                    }
                    Some(i) => match ui.midi.connect_in(i) {
                        Ok(nm) => ui.status = format!("MIDI in: {nm}"),
                        Err(e) => ui.status = format!("MIDI in error: {e}"),
                    },
                }
            }
            let out_lbl = ui
                .midi
                .selected_out
                .and_then(|i| ui.midi.out_ports.get(i))
                .cloned()
                .unwrap_or_else(|| "(none)".into());
            let mut req_out = ui.midi.selected_out;
            egui::ComboBox::from_id_salt("midi_out_sel")
                .selected_text(format!("Out: {out_lbl}"))
                .show_ui(u, |u| {
                    for (i, name) in ui.midi.out_ports.iter().enumerate() {
                        u.selectable_value(&mut req_out, Some(i), name.clone());
                    }
                });
            if req_out != ui.midi.selected_out
                && let Some(i) = req_out
            {
                match ui.midi.connect_out(i) {
                    Ok(nm) => ui.status = format!("MIDI out: {nm}"),
                    Err(e) => ui.status = format!("MIDI out error: {e}"),
                }
            }
            if ui.midi.selected_out.is_some() && u.button("Test Out").clicked() {
                ui.midi.send_note_on(0, 60, 0.8);
                ui.midi.send_note_off(0, 60);
                ui.status = "sent MIDI C4 test".into();
            }
            let tgt_lbl = ui
                .lock_midi_ch
                .and_then(|ci| s.song.channels.get(ci).map(|c| format!("Lock: {}", c.name)))
                .unwrap_or_else(|| "Follow active".into());
            egui::ComboBox::from_id_salt("midi_tgt_lock")
                .selected_text(tgt_lbl)
                .show_ui(u, |u| {
                    u.selectable_value(&mut ui.lock_midi_ch, None, "Follow active");
                    for (ci, c) in s.song.channels.iter().enumerate() {
                        u.selectable_value(&mut ui.lock_midi_ch, Some(ci), c.name.clone());
                    }
                });
            egui::ComboBox::from_id_salt("learn_curve_sel")
                .selected_text(format!("Learn: {}", ui.midi_learn.pending_curve.name()))
                .show_ui(u, |u| {
                    for c in MapCurve::ALL {
                        u.selectable_value(&mut ui.midi_learn.pending_curve, c, c.name());
                    }
                });
            if let Some(armed) = ui.midi_learn.armed_target {
                u.colored_label(
                    egui::Color32::YELLOW,
                    format!("Move CC for {}", armed.label()),
                );
                if u.small_button("Cancel").clicked() {
                    ui.midi_learn.armed_target = None;
                }
            } else if let Some((ch, cc, val)) = ui.midi_learn.last_cc {
                u.label(format!("Last CC ch{} #{}={}", ch + 1, cc, val));
            }
        });
        if !s.song.midi_mappings.is_empty() {
            let mut rm_map = None;
            u.horizontal_wrapped(|u| {
                u.label("Bindings:");
                for (mi, m) in s.song.midi_mappings.iter().enumerate() {
                    u.label(format!(
                        "CC{} -> {} ({})",
                        m.cc,
                        m.target.label(),
                        m.curve.name()
                    ));
                    if u.small_button("×").clicked() {
                        rm_map = Some(mi);
                    }
                }
            });
            if let Some(mi) = rm_map {
                s.song.midi_mappings.remove(mi);
            }
        }
        u.separator();
        u.horizontal_wrapped(|u| {
            u.strong("Audio Input & Punch");
            let ai_lbl = ui
                .selected_audio_in
                .and_then(|i| ui.audio_in_ports.get(i))
                .cloned()
                .unwrap_or_else(|| "(none)".into());
            let mut sel_ai = ui.selected_audio_in;
            egui::ComboBox::from_id_salt("audio_in_sel")
                .selected_text(ai_lbl)
                .show_ui(u, |u| {
                    u.selectable_value(&mut sel_ai, None, "(none)");
                    for (i, nm) in ui.audio_in_ports.iter().enumerate() {
                        u.selectable_value(&mut sel_ai, Some(i), nm.clone());
                    }
                });
            ui.selected_audio_in = sel_ai;
            u.add(
                egui::DragValue::new(&mut ui.audio_in_ch)
                    .range(0..=16)
                    .prefix("In Ch "),
            );
            u.checkbox(&mut ui.audio_in_stereo, "Stereo");
            let is_open = ui.audio_in.is_some();
            if u.button(if is_open { "Close In" } else { "Open In" })
                .clicked()
            {
                if is_open {
                    ui.audio_in = None;
                    s.set_input_consumer(None);
                    ui.status = "audio input closed".into();
                } else if let Some(idx) = ui.selected_audio_in {
                    let (tx, rx) = rtrb::RingBuffer::new(16384);
                    match AudioInputHandle::open(idx, ui.audio_in_ch, ui.audio_in_stereo, tx) {
                        Ok(h) => {
                            ui.status = format!(
                                "audio input open: {} ({:.0} Hz)",
                                h.device_name, h.sample_rate
                            );
                            ui.audio_in = Some(h);
                            s.set_input_consumer(Some(rx));
                        }
                        Err(e) => ui.status = format!("audio input error: {e}"),
                    }
                } else {
                    ui.status = "select an audio input device first".into();
                }
            }
            let mut mon = s.rt.input_monitor;
            let mut mon_ins = s.rt.input_insert;
            if u.toggle_value(&mut mon, "Monitor").changed() {
                s.set_input_monitor(mon, mon_ins);
            }
            let ins_lbl = s
                .song
                .mixer_tracks
                .get(mon_ins)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "Master".into());
            egui::ComboBox::from_id_salt("audio_mon_ins")
                .width(74.0)
                .selected_text(ins_lbl)
                .show_ui(u, |u| {
                    for (mi, mt) in s.song.mixer_tracks.iter().enumerate() {
                        u.selectable_value(&mut mon_ins, mi, mt.name.clone());
                    }
                });
            if mon_ins != s.rt.input_insert {
                s.set_input_monitor(mon, mon_ins);
            }
            let mut rec_only = s.rt.input_rec_only;
            if u.toggle_value(&mut rec_only, "Rec In Only").changed() {
                s.set_input_rec_only(rec_only);
            }
            u.add(
                egui::ProgressBar::new(s.rt.input_peak.clamp(0.0, 1.0))
                    .desired_width(70.0)
                    .text("In"),
            );
            let mut punch = s.song.punch_in_tick.is_some();
            if u.toggle_value(&mut punch, "Punch").changed() {
                if punch {
                    let bar = s.song.time_sig.ticks_per_bar();
                    s.song.punch_in_tick = Some(0);
                    s.song.punch_out_tick = Some(bar);
                } else {
                    s.song.punch_in_tick = None;
                    s.song.punch_out_tick = None;
                }
            }
            if let (Some(pi), Some(po)) = (&mut s.song.punch_in_tick, &mut s.song.punch_out_tick) {
                u.add(egui::DragValue::new(pi).range(0..=4096).prefix("In "));
                u.add(egui::DragValue::new(po).range(4..=8192).prefix("Out "));
            }
        });
        u.separator();
        u.horizontal_wrapped(|u| {
            u.strong("Peak Controllers & LFO");
            if u.button("+ Peak Ctrl").clicked() {
                s.song.peak_controllers.push(PeakController::default());
            }
        });
        let n_mix = s.song.mixer_tracks.len();
        let n_ch = s.song.channels.len();
        let mut rm_pc = None;
        for (pi, pc) in s.song.peak_controllers.iter_mut().enumerate() {
            u.horizontal_wrapped(|u| {
                u.checkbox(&mut pc.enabled, format!("PC{}", pi + 1));
                let mut src = pc.source_insert.min(n_mix.saturating_sub(1));
                u.add(
                    egui::DragValue::new(&mut src)
                        .range(0..=n_mix.saturating_sub(1))
                        .prefix("Src I"),
                );
                pc.source_insert = src;
                let mut tgt_ch = match pc.target {
                    AutoTarget::ChannelVol(c)
                    | AutoTarget::ChannelPan(c)
                    | AutoTarget::ChannelCut(c) => c.min(n_ch.saturating_sub(1)),
                    _ => 0,
                };
                let tgt_kind = match pc.target {
                    AutoTarget::ChannelCut(_) => 1,
                    AutoTarget::InsertVol(_) => 2,
                    AutoTarget::MasterVol => 3,
                    _ => 0,
                };
                let mut kind_sel = tgt_kind;
                egui::ComboBox::from_id_salt(("pc_tgt_kind", pi))
                    .width(74.0)
                    .selected_text(match kind_sel {
                        1 => "Ch Cut",
                        2 => "Ins Vol",
                        3 => "Master",
                        _ => "Ch Vol",
                    })
                    .show_ui(u, |u| {
                        u.selectable_value(&mut kind_sel, 0, "Ch Vol");
                        u.selectable_value(&mut kind_sel, 1, "Ch Cut");
                        u.selectable_value(&mut kind_sel, 2, "Ins Vol");
                        u.selectable_value(&mut kind_sel, 3, "Master");
                    });
                u.add(
                    egui::DragValue::new(&mut tgt_ch)
                        .range(0..=n_mix.max(n_ch).saturating_sub(1))
                        .prefix("#"),
                );
                pc.target = match kind_sel {
                    1 => AutoTarget::ChannelCut(tgt_ch.min(n_ch.saturating_sub(1))),
                    2 => AutoTarget::InsertVol(tgt_ch.min(n_mix.saturating_sub(1))),
                    3 => AutoTarget::MasterVol,
                    _ => AutoTarget::ChannelVol(tgt_ch.min(n_ch.saturating_sub(1))),
                };
                let (lo, hi) = pc.target.range();
                u.add(egui::Slider::new(&mut pc.base, lo..=hi).text("Base"));
                u.add(egui::Slider::new(&mut pc.amount, -1.5..=1.5).text("Amt"));
                u.add(egui::Slider::new(&mut pc.decay_ms, 10.0..=500.0).text("Dec"));
                u.add(egui::Slider::new(&mut pc.lfo_hz, 0.0..=15.0).text("LFO Hz"));
                u.add(egui::Slider::new(&mut pc.lfo_amt, 0.0..=1.0).text("LFO"));
                if u.small_button("×").clicked() {
                    rm_pc = Some(pi);
                }
            });
        }
        if let Some(pi) = rm_pc {
            s.song.peak_controllers.remove(pi);
        }
    });
}

pub fn channel_rack(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.horizontal_wrapped(|u| {
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
        if u.button("+ Super").clicked() {
            let idx = s.song.channels.len();
            let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
            let c = Channel {
                name: format!("Super {}", idx + 1),
                kind: ChannelKind::SuperSynth(SuperSynthCfg::default()),
                color: palette_color(idx),
                cutoff: 4200.0,
                mixer_insert: ins,
                ..Default::default()
            };
            ui.sel_ch = s.song.add_channel(c);
            s.rebuild_channel(ui.sel_ch);
            ui.status = format!("added SuperSynth {}", idx + 1);
        }
        if u.button("+ FM").clicked() {
            let idx = s.song.channels.len();
            let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
            let c = Channel {
                name: format!("FM {}", idx + 1),
                kind: ChannelKind::FmSynth(FmSynthCfg::default()),
                color: palette_color(idx),
                cutoff: 9000.0,
                mixer_insert: ins,
                ..Default::default()
            };
            ui.sel_ch = s.song.add_channel(c);
            s.rebuild_channel(ui.sel_ch);
            ui.status = format!("added FM Synth {}", idx + 1);
        }
        if u.button("+ DrumRack").clicked() {
            let idx = s.song.channels.len();
            let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
            let c = Channel {
                name: format!("Drums {}", idx + 1),
                kind: ChannelKind::DrumRack {
                    pads: default_drum_pads(),
                },
                color: palette_color(idx),
                cutoff: 16000.0,
                mixer_insert: ins,
                ..Default::default()
            };
            ui.sel_ch = s.song.add_channel(c);
            ui.status = format!("added DrumRack {}", idx + 1);
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
        let mut pick_clap: Option<ClapPluginDesc> = None;
        let mut scan_clap = false;
        let mut browse_clap = false;
        egui::ComboBox::from_id_salt("add_clap_ch_combo")
            .selected_text("+ CLAP")
            .width(74.0)
            .show_ui(u, |u| {
                let mut shown = 0usize;
                for desc in ui.clap_catalog.plugins.iter().filter(|d| d.is_instrument) {
                    if u.button(&desc.name).clicked() {
                        pick_clap = Some(desc.clone());
                    }
                    shown += 1;
                }
                if shown == 0 {
                    for desc in &ui.clap_catalog.plugins {
                        if u.button(&desc.name).clicked() {
                            pick_clap = Some(desc.clone());
                        }
                    }
                }
                if !ui.clap_catalog.plugins.is_empty() {
                    u.separator();
                }
                if u.button("Scan CLAP folders").clicked() {
                    scan_clap = true;
                }
                if u.button("Load .clap file...").clicked() {
                    browse_clap = true;
                }
            });
        if scan_clap {
            scan_all_clap_plugins(ui);
        }
        if browse_clap
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("CLAP Plugin", &["clap"])
                .pick_file()
        {
            match register_clap_file_into_catalog(ui, &p) {
                Ok(descs) => {
                    if let Some(d) = descs
                        .iter()
                        .find(|d| d.is_instrument)
                        .or_else(|| descs.first())
                    {
                        add_clap_channel_from_desc(ui, s, d);
                    } else {
                        ui.status = "no CLAP plugins in selected file".into();
                    }
                }
                Err(e) => ui.status = format!("CLAP scan failed: {e}"),
            }
        }
        if let Some(desc) = pick_clap {
            add_clap_channel_from_desc(ui, s, &desc);
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
    let mut drop_row: Option<(Option<usize>, BrowserDragPayload)> = None;
    for i in 0..n {
        let ch_color = color32(s.song.channels[i].color);
        let row_resp = u.horizontal(|u| {
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

            // Kind picker / badge.
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
                ChannelKind::SuperSynth(cfg) => {
                    if u.button(format!("Super {}u", cfg.unison)).clicked() {
                        ui.sel_ch = i;
                        ui.show_ch_settings = true;
                    }
                }
                ChannelKind::FmSynth(cfg) => {
                    if u.button(format!("FM {}", cfg.algo.name())).clicked() {
                        ui.sel_ch = i;
                        ui.show_ch_settings = true;
                    }
                }
                ChannelKind::DrumRack { pads } => {
                    if u.button(format!("Rack ({}p)", pads.len())).clicked() {
                        ui.sel_ch = i;
                        ui.show_ch_settings = true;
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
                ChannelKind::ClapPlugin(cfg) => {
                    let label = if cfg.name.is_empty() {
                        "CLAP".to_string()
                    } else {
                        format!("CLAP: {}", cfg.name)
                    };
                    if u.button(label).clicked() {
                        ui.sel_ch = i;
                        ui.show_ch_settings = true;
                    }
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
                .max_height(30.0)
                .show(u, |u| {
                    let step_w = 24.0;
                    let step_gap = 2.0;
                    let pad_h = 22.0;
                    let total_w = pat_steps as f32 * (step_w + step_gap);
                    let (grid_rect, grid_resp) = u.allocate_exact_size(
                        egui::vec2(total_w, pad_h),
                        egui::Sense::click_and_drag(),
                    );
                    let painter = u.painter_at(grid_rect);
                    let hover_pos = grid_resp.hover_pos();
                    for step in 0..pat_steps {
                        let x = grid_rect.min.x + step as f32 * (step_w + step_gap);
                        let pad = egui::Rect::from_min_size(
                            egui::pos2(x, grid_rect.min.y),
                            egui::vec2(step_w, pad_h),
                        );
                        let active = s.song.step_active(pat, i, step);
                        let playhead = playing_step == Some(step);
                        let beat_alt = (step / beat_steps).is_multiple_of(2);
                        let hov = hover_pos.is_some_and(|p| pad.contains(p));
                        crate::ui_widgets::draw_step_pad(
                            &painter, pad, active, playhead, beat_alt, hov, ch_color,
                        );
                    }
                    let step_at = |x: f32| {
                        (((x - grid_rect.min.x) / (step_w + step_gap)).floor() as usize)
                            .min(pat_steps.saturating_sub(1))
                    };
                    let paint_id = u.make_persistent_id(("step_drag_mode", i));
                    if grid_resp.drag_started_by(egui::PointerButton::Primary)
                        && let Some(pos) = u.ctx().input(|inp| inp.pointer.press_origin())
                    {
                        let st = step_at(pos.x);
                        let mode = !s.song.step_active(pat, i, st);
                        u.ctx().data_mut(|d| d.insert_temp(paint_id, mode));
                        s.song.set_step(pat, i, st, mode);
                    } else if grid_resp.dragged_by(egui::PointerButton::Primary)
                        && let Some(pos) = hover_pos
                    {
                        let st = step_at(pos.x);
                        let mode = u
                            .ctx()
                            .data(|d| d.get_temp::<bool>(paint_id))
                            .unwrap_or(true);
                        s.song.set_step(pat, i, st, mode);
                    } else if grid_resp.dragged_by(egui::PointerButton::Secondary)
                        && let Some(pos) = hover_pos
                    {
                        let st = step_at(pos.x);
                        s.song.set_step(pat, i, st, false);
                    } else if grid_resp.clicked_by(egui::PointerButton::Secondary)
                        && let Some(pos) = hover_pos
                    {
                        let st = step_at(pos.x);
                        s.song.set_step(pat, i, st, false);
                    } else if grid_resp.clicked()
                        && !grid_resp.drag_stopped()
                        && let Some(pos) = hover_pos
                    {
                        let st = step_at(pos.x);
                        s.song.toggle_step(pat, i, st);
                    }
                });
        });
        if let Some(payload) = row_resp
            .response
            .dnd_release_payload::<BrowserDragPayload>()
        {
            drop_row = Some((Some(i), (*payload).clone()));
        }
    }

    let (_, drop_zone_payload) =
        u.dnd_drop_zone::<BrowserDragPayload, _>(egui::Frame::group(u.style()), |u| {
            u.small("+ Drop sample or preset from Browser here to create a new channel");
        });
    if let Some(payload) = drop_zone_payload {
        drop_row = Some((None, (*payload).clone()));
    }
    if let Some((target_ch, payload)) = drop_row {
        drop_browser_onto_channel(ui, s, target_ch, &payload);
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

#[derive(Default)]
struct ClapUiOutput {
    param_updates: Vec<(u32, f64)>,
    reloaded: Option<ClapAudioNode>,
    graph_dirty: bool,
    status: Option<String>,
}

fn clap_cfg_from_desc(desc: &ClapPluginDesc) -> ClapPluginCfg {
    ClapPluginCfg::new(desc.path.clone(), desc.id.clone(), desc.name.clone())
}

fn scan_all_clap_plugins(ui: &mut AppUi) {
    ui.clap_catalog = scan_clap_dirs(&default_clap_dirs());
    save_cached_catalog(&ui.clap_catalog);
    ui.status = format!(
        "scanned {} CLAP plugins ({} errors)",
        ui.clap_catalog.plugins.len(),
        ui.clap_catalog.last_errors.len()
    );
}

fn register_clap_file_into_catalog(
    ui: &mut AppUi,
    path: &Path,
) -> Result<Vec<ClapPluginDesc>, String> {
    let descs = scan_clap_file_isolated(path)?;
    for d in &descs {
        ui.clap_catalog
            .plugins
            .retain(|x| !(x.path == d.path && x.id == d.id));
        ui.clap_catalog.plugins.push(d.clone());
    }
    ui.clap_catalog
        .plugins
        .sort_by_key(|a| a.name.to_lowercase());
    save_cached_catalog(&ui.clap_catalog);
    Ok(descs)
}

fn add_clap_channel_from_desc(ui: &mut AppUi, s: &mut AppState, desc: &ClapPluginDesc) -> usize {
    let idx = s.song.channels.len();
    let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
    let c = Channel {
        name: desc.name.clone(),
        kind: ChannelKind::ClapPlugin(clap_cfg_from_desc(desc)),
        color: palette_color(idx),
        cutoff: 12000.0,
        mixer_insert: ins,
        ..Default::default()
    };
    let ch = s.song.add_channel(c);
    sync_clap_plugins(ui, s);
    ui.sel_ch = ch;
    ui.show_ch_settings = true;
    ui.status = format!("added CLAP {}", desc.name);
    ch
}

fn add_clap_fx_from_desc(
    ui: &mut AppUi,
    s: &mut AppState,
    ins: usize,
    desc: &ClapPluginDesc,
) -> bool {
    let slot = EffectSlot::new(FxKind::ClapPlugin(clap_cfg_from_desc(desc)));
    let added = if let Some(track) = s.song.mixer_tracks.get_mut(ins) {
        if track.add_fx(slot) {
            ui.sel_fx_slot = track.fx.len().saturating_sub(1);
            ui.status = format!("added CLAP {} to {}", desc.name, track.name);
            true
        } else {
            ui.status = "FX rack full".into();
            false
        }
    } else {
        false
    };
    if added {
        sync_clap_plugins(ui, s);
    }
    added
}

fn sync_clap_slot(
    cfg: &mut ClapPluginCfg,
    clap: &mut ClapManager,
    sr: f32,
    active: &mut Vec<u64>,
    to_mount: &mut Vec<(u64, ClapAudioNode)>,
    graph_dirty: &mut bool,
) {
    active.push(cfg.rt_id);
    let prev_lat = cfg.reported_latency;
    if let Some(node) = clap.ensure_slot(cfg, sr) {
        to_mount.push((cfg.rt_id, node));
        *graph_dirty = true;
    }
    if let Some(slot) = clap.slots.get_mut(&cfg.rt_id)
        && let Some(main) = slot.main.as_mut()
    {
        let (dirty, changed) = main.poll_callbacks();
        if dirty && let Ok(b64) = main.save_state_b64() {
            cfg.state_b64 = b64;
        }
        if changed {
            for (pid, pv) in &mut cfg.params {
                if let Some(info) = main.params.iter().find(|p| p.id == *pid) {
                    *pv = info.value;
                }
            }
            if cfg.reported_latency != main.latency_samples {
                cfg.reported_latency = main.latency_samples;
                *graph_dirty = true;
            }
        }
    }
    if cfg.reported_latency != prev_lat {
        *graph_dirty = true;
    }
}

pub fn sync_clap_plugins(ui: &mut AppUi, s: &mut AppState) {
    let sr = s.rt.sr;
    let mut active = Vec::new();
    let mut to_mount = Vec::new();
    let mut graph_dirty = false;

    for ch in &mut s.song.channels {
        if let ChannelKind::ClapPlugin(cfg) = &mut ch.kind {
            sync_clap_slot(
                cfg,
                &mut ui.clap,
                sr,
                &mut active,
                &mut to_mount,
                &mut graph_dirty,
            );
        }
    }
    for track in &mut s.song.mixer_tracks {
        for slot in &mut track.fx {
            if let FxKind::ClapPlugin(cfg) = &mut slot.kind {
                sync_clap_slot(
                    cfg,
                    &mut ui.clap,
                    sr,
                    &mut active,
                    &mut to_mount,
                    &mut graph_dirty,
                );
            }
        }
    }

    let stale: Vec<u64> = ui
        .clap
        .slots
        .keys()
        .copied()
        .filter(|id| !active.contains(id))
        .collect();
    for id in stale {
        s.unmount_clap(id);
        ui.clap.slots.remove(&id);
        graph_dirty = true;
    }
    for (id, node) in to_mount {
        s.mount_clap(id, node);
    }
    if graph_dirty {
        s.rt.ensure_channels(&s.song);
    }
}

fn render_clap_editor_ui(
    cfg: &mut ClapPluginCfg,
    clap: &mut ClapManager,
    parent: Option<RawWindowHandle>,
    filter: &mut String,
    sr: f32,
    u: &mut egui::Ui,
) -> ClapUiOutput {
    let mut out = ClapUiOutput::default();
    let mut want_reload = false;
    let mut load_err: Option<String> = None;

    u.separator();
    u.horizontal_wrapped(|u| {
        u.strong(format!("CLAP: {} ({})", cfg.name, cfg.plugin_id));
        u.small(format!("Latency: {} smp", cfg.reported_latency));
        if let Some(slot) = clap.slots.get_mut(&cfg.rt_id) {
            if let Some(main) = slot.main.as_mut() {
                let gui_lbl = if main.gui_open {
                    "Hide GUI"
                } else {
                    "Open GUI"
                };
                if u.button(gui_lbl).clicked() {
                    if main.gui_open {
                        main.close_gui();
                    } else if let Err(e) = main.open_gui(parent, &cfg.name) {
                        main.gui_error = Some(e);
                    }
                }
                if u.button("Save State").clicked() {
                    match main.save_state_b64() {
                        Ok(b64) => {
                            cfg.state_b64 = b64;
                            out.status = Some(format!("saved CLAP state for {}", cfg.name));
                        }
                        Err(e) => {
                            out.status = Some(format!("save state failed: {e}"));
                        }
                    }
                }
                if u.button("Reload").clicked() {
                    want_reload = true;
                }
                if let Some(err) = &main.gui_error {
                    u.colored_label(egui::Color32::from_rgb(230, 140, 80), format!("GUI: {err}"));
                }
            } else if let Some(err) = &slot.load_error {
                load_err = Some(err.clone());
            }
        } else {
            u.small("CLAP slot pending sync...");
            if u.button("Load Now").clicked() {
                want_reload = true;
            }
        }
    });

    if let Some(err) = load_err {
        u.horizontal_wrapped(|u| {
            u.colored_label(
                egui::Color32::from_rgb(230, 100, 90),
                format!("Load error: {err}"),
            );
            if u.button("Retry Load").clicked() {
                want_reload = true;
            }
        });
    }

    if let Some(slot) = clap.slots.get_mut(&cfg.rt_id)
        && let Some(main) = slot.main.as_mut()
        && !main.params.is_empty()
    {
        u.horizontal(|u| {
            u.label("Params:");
            u.add(egui::TextEdit::singleline(filter).desired_width(140.0));
        });
        let q = filter.trim().to_ascii_lowercase();
        egui::ScrollArea::vertical()
            .id_salt(("clap_params", cfg.rt_id))
            .max_height(210.0)
            .show(u, |u| {
                for p in &mut main.params {
                    if p.hidden {
                        continue;
                    }
                    if !q.is_empty()
                        && !p.name.to_ascii_lowercase().contains(&q)
                        && !p.module.to_ascii_lowercase().contains(&q)
                    {
                        continue;
                    }
                    u.horizontal(|u| {
                        let lbl = if p.module.is_empty() {
                            p.name.clone()
                        } else {
                            format!("{}: {}", p.module, p.name)
                        };
                        u.label(lbl);
                        if p.readonly {
                            u.small(&p.text);
                        } else {
                            let mut slider =
                                egui::Slider::new(&mut p.value, p.min..=p.max).show_value(false);
                            if p.stepped {
                                slider = slider.step_by(1.0);
                            }
                            if u.add(slider).changed() {
                                p.text = format!("{:.2}", p.value);
                                out.param_updates.push((p.id, p.value));
                            }
                            u.small(&p.text);
                            if u.small_button("R")
                                .on_hover_text("Reset to default")
                                .clicked()
                            {
                                p.value = p.default.clamp(p.min, p.max);
                                p.text = format!("{:.2}", p.value);
                                out.param_updates.push((p.id, p.value));
                            }
                        }
                    });
                }
            });
    }

    for &(pid, val) in &out.param_updates {
        if let Some(existing) = cfg.params.iter_mut().find(|(id, _)| *id == pid) {
            existing.1 = val;
        } else {
            cfg.params.push((pid, val));
        }
    }

    if want_reload {
        let prev_lat = cfg.reported_latency;
        out.reloaded = clap.reload_slot(cfg, sr);
        if cfg.reported_latency != prev_lat || out.reloaded.is_some() {
            out.graph_dirty = true;
        }
    }
    out
}

fn channel_settings_panel(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let sr = s.rt.sr;
    let mut need_rebuild = false;
    let mut need_cutoff = None;
    let mut trigger_pad_note: Option<u8> = None;
    let mut ctx_act: Option<ParamCtxAction> = None;
    let mut open_edison: Option<usize> = None;
    let mut clap_out: Option<(u64, ClapUiOutput)> = None;
    let mix_names: Vec<String> = s.song.mixer_tracks.iter().map(|m| m.name.clone()).collect();
    let sample_names: Vec<String> = s.song.samples.iter().map(|sm| sm.name.clone()).collect();
    let presets = factory_presets();

    let Some(c) = s.song.channels.get_mut(ch) else {
        return;
    };
    u.group(|u| {
        u.horizontal_wrapped(|u| {
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
        u.horizontal_wrapped(|u| {
            let old_spec = c.voice_spec();
            let cut_r = u.add(
                egui::Slider::new(&mut c.cutoff, 80.0..=16000.0)
                    .logarithmic(true)
                    .text("Cut"),
            );
            collect_param_ctx(&cut_r, AutoTarget::ChannelCut(ch), &mut ctx_act);
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
        u.horizontal_wrapped(|u| {
            u.label("Cut group:");
            let mut cg = c.cut_group.unwrap_or(0);
            u.add(egui::DragValue::new(&mut cg).range(0..=8));
            c.cut_group = (cg > 0).then_some(cg);
            u.label("Cut by:");
            let mut cb = c.cut_by.unwrap_or(0);
            u.add(egui::DragValue::new(&mut cb).range(0..=8));
            c.cut_by = (cb > 0).then_some(cb);
            let vol_r = u.add(egui::Slider::new(&mut c.volume, 0.0..=1.5).text("Vol"));
            collect_param_ctx(&vol_r, AutoTarget::ChannelVol(ch), &mut ctx_act);
            let pan_r = u.add(egui::Slider::new(&mut c.pan, -1.0..=1.0).text("Pan"));
            collect_param_ctx(&pan_r, AutoTarget::ChannelPan(ch), &mut ctx_act);
            u.add(egui::Slider::new(&mut c.delay_send, 0.0..=1.0).text("Dly"));
            u.add(egui::Slider::new(&mut c.reverb_send, 0.0..=1.0).text("Verb"));
        });

        // Presets bar (Factory + Save/Load JSON).
        u.horizontal_wrapped(|u| {
            u.label("Preset:");
            egui::ComboBox::from_id_salt("ch_factory_preset")
                .selected_text("Factory...")
                .show_ui(u, |u| {
                    for preset in &presets {
                        if u.button(&preset.name).clicked() {
                            apply_preset_to_channel(c, preset);
                            need_rebuild = true;
                            ui.status = format!("loaded preset {}", preset.name);
                        }
                    }
                });
            if u.button("Save Preset...").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("Thunder Preset", &["json"])
                    .set_file_name(format!("{}.json", c.name))
                    .save_file()
            {
                let pstr = p.to_string_lossy();
                match save_preset_json(c, &pstr) {
                    Ok(()) => ui.status = format!("saved preset {pstr}"),
                    Err(e) => ui.status = format!("preset save failed: {e}"),
                }
            }
            if u.button("Load Preset...").clicked()
                && let Some(p) = rfd::FileDialog::new()
                    .add_filter("Thunder Preset", &["json"])
                    .pick_file()
            {
                match load_preset_json(&p.to_string_lossy()) {
                    Ok(preset) => {
                        apply_preset_to_channel(c, &preset);
                        need_rebuild = true;
                        ui.status = format!("loaded preset {}", preset.name);
                    }
                    Err(e) => ui.status = format!("preset load failed: {e}"),
                }
            }
        });

        // Instrument-specific editor.
        match &mut c.kind {
            ChannelKind::Sampler { sample } => {
                u.separator();
                if let Some(s_idx) = *sample
                    && let Some(smp) = s.song.samples.get_mut(s_idx)
                {
                    let peaks = &smp.peaks;
                    let w = u.available_width().clamp(240.0, 520.0);
                    let (wrect, _) =
                        u.allocate_exact_size(egui::vec2(w, 56.0), egui::Sense::hover());
                    let wp = u.painter_at(wrect);
                    wp.rect_filled(wrect, 2.0, egui::Color32::from_gray(16));
                    let tx0 = wrect.min.x + c.sampler.trim_start * wrect.width();
                    let tx1 = wrect.min.x + c.sampler.trim_end * wrect.width();
                    wp.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(tx0, wrect.min.y),
                            egui::pos2(tx1.max(tx0 + 2.0), wrect.max.y),
                        ),
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(60, 120, 180, 35),
                    );
                    if c.sampler.loop_enabled {
                        let span = (tx1 - tx0).max(1.0);
                        let lx0 = tx0 + c.sampler.loop_start * span;
                        let lx1 = tx0 + c.sampler.loop_end * span;
                        wp.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(lx0, wrect.min.y),
                                egui::pos2(lx1.max(lx0 + 2.0), wrect.max.y),
                            ),
                            0.0,
                            egui::Color32::from_rgba_unmultiplied(120, 210, 100, 45),
                        );
                    }
                    let mid_y = wrect.center().y;
                    for (bi, &pk) in peaks.iter().enumerate() {
                        let bx =
                            wrect.min.x + (bi as f32 / peaks.len().max(1) as f32) * wrect.width();
                        let half_h = (pk.clamp(0.0, 1.0) * (wrect.height() * 0.45)).max(1.0);
                        wp.line_segment(
                            [
                                egui::pos2(bx, mid_y - half_h),
                                egui::pos2(bx, mid_y + half_h),
                            ],
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 195, 240)),
                        );
                    }
                    u.horizontal_wrapped(|u| {
                        let ch_lbl = if smp.is_stereo() { "Stereo" } else { "Mono" };
                        u.label(format!(
                            "{} ({ch_lbl}, {:.0} Hz, {} smp)",
                            smp.name,
                            smp.sr,
                            smp.data.len()
                        ));
                        if u.button("Normalize").clicked() {
                            smp.normalize_in_place();
                            ui.status = "normalized sample".into();
                        }
                        if u.button("Remove DC").clicked() {
                            smp.remove_dc_in_place();
                            ui.status = "removed DC offset".into();
                        }
                        if u.button("Reverse PCM").clicked() {
                            smp.reverse_in_place();
                            ui.status = "reversed PCM data".into();
                        }
                        if u.button("Edison...").clicked() {
                            open_edison = Some(s_idx);
                        }
                    });
                } else {
                    u.label("Load a sample from the Browser to view waveform and edit trim/loop.");
                }
                u.horizontal_wrapped(|u| {
                    u.add(egui::Slider::new(&mut c.sampler.trim_start, 0.0..=0.95).text("Trim S"));
                    u.add(egui::Slider::new(&mut c.sampler.trim_end, 0.05..=1.0).text("Trim E"));
                    u.checkbox(&mut c.sampler.reverse, "Reverse");
                    u.selectable_value(&mut c.sampler.stretch, StretchMode::Resample, "Resample");
                    u.selectable_value(&mut c.sampler.stretch, StretchMode::Granular, "Granular");
                });
                u.horizontal_wrapped(|u| {
                    u.checkbox(&mut c.sampler.loop_enabled, "Loop");
                    u.add(egui::Slider::new(&mut c.sampler.loop_start, 0.0..=0.9).text("Loop S"));
                    u.add(egui::Slider::new(&mut c.sampler.loop_end, 0.1..=1.0).text("Loop E"));
                    u.add(egui::Slider::new(&mut c.sampler.loop_xfade, 0.0..=0.45).text("Xfade"));
                    u.add(
                        egui::Slider::new(&mut c.sampler.filter_env_amt, -1.0..=1.0).text("FltEnv"),
                    );
                });
            }
            ChannelKind::SuperSynth(cfg) => {
                u.separator();
                u.horizontal_wrapped(|u| {
                    let mut uni = cfg.unison;
                    u.add(egui::Slider::new(&mut uni, 1..=7).text("Unison"));
                    cfg.unison = uni;
                    u.add(egui::Slider::new(&mut cfg.detune, 0.0..=1.0).text("Detune"));
                    u.add(egui::Slider::new(&mut cfg.spread, 0.0..=1.0).text("Spread"));
                    u.add(egui::Slider::new(&mut cfg.sub_level, 0.0..=1.0).text("Sub"));
                    u.add(egui::Slider::new(&mut cfg.noise_level, 0.0..=0.8).text("Noise"));
                    u.checkbox(&mut cfg.sync_osc2, "Sync2");
                    u.checkbox(&mut cfg.ring_mod, "RingMod");
                });
                u.horizontal_wrapped(|u| {
                    egui::ComboBox::from_id_salt("super_flt_mode")
                        .selected_text(format!("Filter: {}", cfg.filter_mode.name()))
                        .show_ui(u, |u| {
                            for fm in FilterMode::ALL {
                                u.selectable_value(&mut cfg.filter_mode, fm, fm.name());
                            }
                        });
                    u.add(egui::Slider::new(&mut cfg.drive, 0.0..=1.0).text("Drive"));
                    u.add(egui::Slider::new(&mut cfg.keytrack, 0.0..=1.0).text("Keytrack"));
                    u.add(egui::Slider::new(&mut cfg.filter_env, -1.0..=1.0).text("FltEnv"));
                });
                for (oi, osc) in cfg.oscs.iter_mut().enumerate() {
                    u.horizontal_wrapped(|u| {
                        u.label(format!("Osc {}:", oi + 1));
                        egui::ComboBox::from_id_salt(format!("super_osc_w_{oi}"))
                            .width(68.0)
                            .selected_text(osc.wave.name())
                            .show_ui(u, |u| {
                                for w in WaveKind::all() {
                                    u.selectable_value(&mut osc.wave, w, w.name());
                                }
                            });
                        let mut semi = osc.semi;
                        u.add(
                            egui::DragValue::new(&mut semi)
                                .range(-24..=24)
                                .suffix(" st"),
                        );
                        osc.semi = semi;
                        u.add(egui::Slider::new(&mut osc.cents, -50.0..=50.0).text("ct"));
                        u.add(egui::Slider::new(&mut osc.level, 0.0..=1.2).text("Lvl"));
                    });
                }
                for (li, lfo) in cfg.lfos.iter_mut().enumerate() {
                    u.horizontal_wrapped(|u| {
                        u.label(format!("LFO {}:", li + 1));
                        egui::ComboBox::from_id_salt(format!("super_lfo_w_{li}"))
                            .width(64.0)
                            .selected_text(lfo.wave.name())
                            .show_ui(u, |u| {
                                for w in WaveKind::all() {
                                    u.selectable_value(&mut lfo.wave, w, w.name());
                                }
                            });
                        egui::ComboBox::from_id_salt(format!("super_lfo_t_{li}"))
                            .width(68.0)
                            .selected_text(lfo.target.name())
                            .show_ui(u, |u| {
                                for t in ModTarget::ALL {
                                    u.selectable_value(&mut lfo.target, t, t.name());
                                }
                            });
                        u.add(egui::Slider::new(&mut lfo.rate_hz, 0.1..=20.0).text("Hz"));
                        u.add(egui::Slider::new(&mut lfo.depth, 0.0..=1.0).text("Depth"));
                    });
                }
            }
            ChannelKind::FmSynth(cfg) => {
                u.separator();
                u.horizontal(|u| {
                    u.label("FM Algorithm:");
                    egui::ComboBox::from_id_salt("fm_algo_sel")
                        .selected_text(cfg.algo.name())
                        .show_ui(u, |u| {
                            for a in FmAlgo::ALL {
                                u.selectable_value(&mut cfg.algo, a, a.name());
                            }
                        });
                });
                for (oi, op) in cfg.ops.iter_mut().enumerate() {
                    u.horizontal_wrapped(|u| {
                        u.label(format!("Op {}:", oi + 1));
                        u.add(egui::Slider::new(&mut op.ratio, 0.25..=12.0).text("Ratio"));
                        u.add(egui::Slider::new(&mut op.level, 0.0..=1.5).text("Lvl"));
                        u.add(egui::Slider::new(&mut op.decay, 0.03..=4.0).text("Dec"));
                        u.add(egui::Slider::new(&mut op.feedback, 0.0..=1.2).text("FB"));
                    });
                }
            }
            ChannelKind::DrumRack { pads } => {
                u.separator();
                if pads.is_empty() {
                    *pads = default_drum_pads();
                }
                ui.sel_pad = ui.sel_pad.min(pads.len() - 1);
                u.horizontal_top(|u| {
                    u.vertical(|u| {
                        u.label("4x4 Pads (click to trigger & edit):");
                        for row in 0..4 {
                            u.horizontal(|u| {
                                for col in 0..4 {
                                    let pidx = (3 - row) * 4 + col;
                                    if let Some(p) = pads.get(pidx) {
                                        let sel = ui.sel_pad == pidx;
                                        let btn =
                                            egui::Button::new(format!("{}\n({})", p.name, p.note))
                                                .min_size(egui::vec2(72.0, 34.0))
                                                .fill(if sel {
                                                    egui::Color32::from_rgb(70, 115, 165)
                                                } else {
                                                    egui::Color32::from_gray(42)
                                                });
                                        if u.add(btn).clicked() {
                                            ui.sel_pad = pidx;
                                            trigger_pad_note = Some(p.note);
                                        }
                                    }
                                }
                            });
                        }
                    });
                    if let Some(pad) = pads.get_mut(ui.sel_pad) {
                        u.vertical(|u| {
                            u.label(format!("Pad #{} Inspector", ui.sel_pad + 1));
                            u.horizontal(|u| {
                                u.add(
                                    egui::TextEdit::singleline(&mut pad.name).desired_width(90.0),
                                );
                                let mut note = pad.note;
                                u.add(
                                    egui::DragValue::new(&mut note)
                                        .range(24..=96)
                                        .prefix("Note: "),
                                );
                                pad.note = note;
                            });
                            u.add(egui::Slider::new(&mut pad.gain, 0.0..=2.0).text("Gain"));
                            u.add(egui::Slider::new(&mut pad.pan, -1.0..=1.0).text("Pan"));
                            u.add(
                                egui::Slider::new(&mut pad.tune_semi, -24.0..=24.0).text("Tune st"),
                            );
                            u.horizontal(|u| {
                                let mut cg = pad.cut_group.unwrap_or(0);
                                u.add(
                                    egui::DragValue::new(&mut cg)
                                        .range(0..=8)
                                        .prefix("CutGrp: "),
                                );
                                pad.cut_group = (cg > 0).then_some(cg);
                                let mut cb = pad.cut_by.unwrap_or(0);
                                u.add(egui::DragValue::new(&mut cb).range(0..=8).prefix("CutBy: "));
                                pad.cut_by = (cb > 0).then_some(cb);
                                let mut off = pad.mixer_offset;
                                u.add(
                                    egui::DragValue::new(&mut off)
                                        .range(-8..=16)
                                        .prefix("MixOff: "),
                                );
                                pad.mixer_offset = off;
                            });
                            if let Some(layer) = pad.layers.first_mut() {
                                u.horizontal(|u| {
                                    let s_lbl = layer
                                        .sample
                                        .and_then(|si| sample_names.get(si))
                                        .cloned()
                                        .unwrap_or_else(|| "Kit fallback".into());
                                    egui::ComboBox::from_id_salt("pad_layer_smp")
                                        .selected_text(s_lbl)
                                        .show_ui(u, |u| {
                                            if u.selectable_label(
                                                layer.sample.is_none(),
                                                "Kit fallback",
                                            )
                                            .clicked()
                                            {
                                                layer.sample = None;
                                            }
                                            for (si, sn) in sample_names.iter().enumerate() {
                                                if u.selectable_label(layer.sample == Some(si), sn)
                                                    .clicked()
                                                {
                                                    layer.sample = Some(si);
                                                }
                                            }
                                        });
                                    let mut kit = layer.kit_fallback.unwrap_or(KitPiece::Kick);
                                    egui::ComboBox::from_id_salt("pad_layer_kit")
                                        .selected_text(kit.name())
                                        .show_ui(u, |u| {
                                            for k in KitPiece::all() {
                                                u.selectable_value(&mut kit, k, k.name());
                                            }
                                        });
                                    layer.kit_fallback = Some(kit);
                                });
                            }
                        });
                    }
                });
            }
            ChannelKind::ClapPlugin(cfg) => {
                let out = render_clap_editor_ui(
                    cfg,
                    &mut ui.clap,
                    ui.parent_window,
                    &mut ui.clap_param_filter,
                    sr,
                    u,
                );
                clap_out = Some((cfg.rt_id, out));
            }
            ChannelKind::Synth { .. } | ChannelKind::Kit { .. } => {}
        }
    });
    if let Some((rt_id, co)) = clap_out {
        if let Some(msg) = co.status {
            ui.status = msg;
        }
        if let Some(node) = co.reloaded {
            s.mount_clap(rt_id, node);
        }
        for (pid, val) in co.param_updates {
            s.set_clap_param(rt_id, pid, val);
        }
        if co.graph_dirty {
            s.rt.ensure_channels(&s.song);
        }
    }
    if need_rebuild {
        s.rebuild_channel(ch);
    } else if let Some(cut) = need_cutoff {
        s.set_channel_cutoff(ch, cut);
    }
    if let Some(note) = trigger_pad_note {
        s.live_note_ch(ch, note);
    }
    if let Some(act) = ctx_act {
        apply_param_ctx(ui, s, act);
    }
    if let Some(s_idx) = open_edison {
        ui.edison_sample = Some(s_idx);
        ui.edison_sel = (0.0, 1.0);
        ui.edison_slices.clear();
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
        ChannelKind::Synth { .. }
        | ChannelKind::SuperSynth(_)
        | ChannelKind::FmSynth(_)
        | ChannelKind::ClapPlugin(_) => s.live_note_ch(i, s.song.channels[i].midi_base),
        ChannelKind::DrumRack { pads } => {
            let note = pads.first().map(|p| p.note).unwrap_or(36);
            s.live_note_ch(i, note);
        }
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
const PIANO_CELL_H: f32 = 15.0;
const PIANO_RULER_H: f32 = 20.0;

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

/// Add a single note (optional slide) or a chord stamp, then report it in the status line.
fn place_note(
    ui: &mut AppUi,
    s: &mut AppState,
    ch: usize,
    pat: usize,
    tick: u32,
    midi: u8,
    cell_w: f32,
) {
    let Some(pat_data) = s.song.patterns.get_mut(pat) else {
        return;
    };
    let max_ticks = pat_data.len_ticks().max(1);
    let len = ui
        .snap_grid
        .default_len(cell_w)
        .min(max_ticks - tick)
        .max(1);
    let Some(notes) = pat_data.notes.get_mut(ch) else {
        return;
    };
    if ui.chord == ChordKind::Off {
        let mut n = Note::new_in(tick, len, midi, 0.85, max_ticks);
        n.slide = ui.next_slide;
        notes.push(n);
        notes.sort_by_key(|x| x.tick);
        ui.sel_notes = notes
            .iter()
            .rposition(|x| x.tick == tick && x.midi == midi)
            .into_iter()
            .collect();
        ui.status = format!(
            "{}note {} at tick {tick}",
            if ui.next_slide { "slide " } else { "" },
            note_label(midi)
        );
    } else {
        let chord_notes = tool_chord_ext(tick, midi, ui.chord, ui.chord_inv, len, max_ticks);
        let count = chord_notes.len();
        notes.extend(chord_notes);
        notes.sort_by_key(|x| x.tick);
        ui.sel_notes.clear();
        ui.status = format!("{} chord ({count}) at tick {tick}", ui.chord.name());
    }
}

fn open_piano_modal(ui: &mut AppUi, s: &mut AppState, ch: usize, pat: usize, kind: PianoModalKind) {
    let backup = s
        .song
        .patterns
        .get(pat)
        .and_then(|p| p.notes.get(ch))
        .cloned()
        .unwrap_or_default();
    if backup.is_empty() {
        ui.status = "no notes to process".into();
        return;
    }
    let step = ui.snap_grid.step_ticks(8.0).max(1.0);
    let pat_max = s
        .song
        .patterns
        .get(pat)
        .map(|p| p.len_ticks())
        .unwrap_or(64);
    let mut modal = PianoModal {
        kind,
        ch,
        pat,
        backup,
        sel: ui.sel_notes.clone(),
        quant: QuantizeCfg {
            grid: step,
            strength: 1.0,
            swing: 0.0,
            max_ticks: pat_max,
        },
        strum: StrumCfg {
            amount: 2,
            vel_slope: 0.0,
            preserve_end: true,
            max_ticks: pat_max,
        },
        flam_gap: 2,
        flam_vel: 0.55,
        arp: ArpCfg {
            step_ticks: TICKS_PER_STEP,
            gate: 0.9,
            octaves: 1,
            dir: ArpDir::Up,
            max_ticks: pat_max,
        },
        rand: RandCfg {
            pitch_range: 0,
            vel_amt: 0.15,
            pan_amt: 0.1,
            seed: 42,
        },
        flip_h: true,
        flip_v: false,
    };
    apply_piano_modal_preview(s, &modal);
    modal.kind = kind;
    ui.modal = Some(modal);
}

fn apply_piano_modal_preview(s: &mut AppState, m: &PianoModal) {
    let Some(pat_data) = s.song.patterns.get_mut(m.pat) else {
        return;
    };
    let max_ticks = pat_data.len_ticks().max(1);
    let Some(notes) = pat_data.notes.get_mut(m.ch) else {
        return;
    };
    *notes = m.backup.clone();
    match m.kind {
        PianoModalKind::Quantize => {
            let mut cfg = m.quant;
            cfg.max_ticks = max_ticks;
            tool_quantize_opts(notes, &m.sel, &cfg);
        }
        PianoModalKind::Strum => {
            let mut cfg = m.strum;
            cfg.max_ticks = max_ticks;
            tool_strum_opts(notes, &m.sel, &cfg);
        }
        PianoModalKind::Flam => tool_flam(notes, &m.sel, m.flam_gap, m.flam_vel),
        PianoModalKind::Arp => {
            let mut cfg = m.arp;
            cfg.max_ticks = max_ticks;
            tool_riff_arp(notes, &cfg);
        }
        PianoModalKind::Randomize => tool_randomize(notes, &m.sel, &m.rand),
        PianoModalKind::Flip => tool_flip(notes, &m.sel, m.flip_h, m.flip_v),
    }
}

pub fn piano_roll(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    piano_toolbar(ui, s, u);
    // Target switch above may have changed the channel: re-resolve.
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let pat = ui.sel_pat;
    piano_options(ui, u);
    piano_canvas(ui, s, ch, pat, u);
    note_property_lane(ui, s, ch, pat, u);
    automation_lane(ui, s, ch, pat, u);
    note_dialog(ui, s, ch, pat, u);
    piano_tool_modal(ui, s, u);
}

fn piano_toolbar(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
    let pat = ui.sel_pat;
    u.horizontal_wrapped(|u| {
        u.heading(format!(
            "Piano roll : {} / P{}",
            s.song.channels[ch].name,
            pat + 1
        ));
        for tool in PianoTool::ALL {
            if u.selectable_label(ui.tool == tool, tool.name())
                .on_hover_text(match tool {
                    PianoTool::Draw => "Draw: click adds/selects notes, drag moves/resizes",
                    PianoTool::Paint => "Paint: drag stamps notes sequentially across steps",
                    PianoTool::Slice => "Slice: split notes at cursor tick",
                    PianoTool::Mute => "Mute: click or drag to mute/unmute notes",
                    PianoTool::Erase => "Erase: click or drag-hover deletes notes",
                    PianoTool::Hear => "Hear: preview pitches without editing",
                })
                .clicked()
            {
                ui.focus = FocusPanel::PianoRoll;
                ui.tool = tool;
            }
        }
        u.toggle_value(&mut ui.next_slide, "Slide")
            .on_hover_text("Toggle FL-style pitch slide for newly placed notes");
        let mut target = ch;
        egui::ComboBox::from_id_salt("target_ch")
            .selected_text(s.song.channels[ch].name.clone())
            .show_ui(u, |u| {
                for (i, c) in s.song.channels.iter().enumerate() {
                    let has_notes = s.song.patterns[pat]
                        .notes
                        .get(i)
                        .map(|ns| !ns.is_empty())
                        .unwrap_or(false);
                    u.selectable_value(
                        &mut target,
                        i,
                        format!("{}{}", if has_notes { "● " } else { "" }, c.name),
                    );
                }
            });
        if target != ch {
            ui.sel_ch = target;
            ui.sel_notes.clear();
            ui.prop_note = None;
            return;
        }
        if u.button("Copy")
            .on_hover_text("Copy selected/channel notes (Ctrl/Cmd+C)")
            .clicked()
        {
            copy_notes(ui, s);
        }
        if u.add_enabled(!ui.note_clipboard.is_empty(), egui::Button::new("Paste"))
            .on_hover_text("Paste notes (Ctrl/Cmd+V)")
            .clicked()
        {
            paste_notes(ui, s);
        }
        if u.button("Dup")
            .on_hover_text("Duplicate selection after end (Ctrl/Cmd+B)")
            .clicked()
        {
            duplicate_selected_notes(ui, s);
        }
        if u.button("Legato")
            .on_hover_text("Extend notes to next start (Ctrl/Cmd+L when focused)")
            .clicked()
        {
            legato_selected_notes(ui, s);
        }
        if u.button("Clone P")
            .on_hover_text("Copy this pattern to the next empty pattern slot")
            .clicked()
        {
            duplicate_pattern(ui, s);
        }
        if u.button("Quant").clicked() {
            open_piano_modal(ui, s, ch, pat, PianoModalKind::Quantize);
        }
        if u.button("Strum").clicked() {
            open_piano_modal(ui, s, ch, pat, PianoModalKind::Strum);
        }
        if u.button("Flam").clicked() {
            open_piano_modal(ui, s, ch, pat, PianoModalKind::Flam);
        }
        if u.button("Arp").clicked() {
            open_piano_modal(ui, s, ch, pat, PianoModalKind::Arp);
        }
        if u.button("Rand").clicked() {
            open_piano_modal(ui, s, ch, pat, PianoModalKind::Randomize);
        }
        if u.button("Flip").clicked() {
            open_piano_modal(ui, s, ch, pat, PianoModalKind::Flip);
        }
        egui::ComboBox::from_id_salt("chord_stamp")
            .selected_text(format!("Chord: {}", ui.chord.name()))
            .show_ui(u, |u| {
                for ck in ChordKind::ALL {
                    u.selectable_value(&mut ui.chord, ck, ck.name());
                }
            });
        if ui.chord != ChordKind::Off {
            u.add(
                egui::DragValue::new(&mut ui.chord_inv)
                    .range(0..=3)
                    .prefix("Inv:"),
            );
        }
        if u.button("Clear").clicked()
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
        {
            notes.clear();
            ui.sel_notes.clear();
        }
        if u.button("Oct-").clicked()
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
    u.horizontal_wrapped(|u| {
        u.label("Snap:");
        egui::ComboBox::from_id_salt("snap_grid")
            .selected_text(ui.snap_grid.name())
            .show_ui(u, |u| {
                for sn in SnapGrid::ALL {
                    u.selectable_value(&mut ui.snap_grid, sn, sn.name());
                }
            });
        u.label("Scale:");
        egui::ComboBox::from_id_salt("scale")
            .selected_text(ui.scale.name())
            .show_ui(u, |u| {
                for cand in ScaleKind::ALL {
                    u.selectable_value(&mut ui.scale, cand, cand.name());
                }
            });
        u.label("Root:");
        egui::ComboBox::from_id_salt("root")
            .selected_text(NOTE_NAMES[(ui.root % 12) as usize])
            .show_ui(u, |u| {
                for r in 0u8..12 {
                    u.selectable_value(&mut ui.root, r, NOTE_NAMES[r as usize]);
                }
            });
        u.toggle_value(&mut ui.snap_scale, "ScaleSnap")
            .on_hover_text("Force added and moved notes into the scale");
        u.toggle_value(&mut ui.show_ghosts, "Ghosts");
        if ui.show_ghosts {
            u.toggle_value(&mut ui.editable_ghosts, "EditGhosts")
                .on_hover_text("Double-click ghost note to switch channel");
        }
        u.add(egui::Slider::new(&mut ui.piano_zoom_x, 1.0..=4.0).text("ZoomX"));
        u.add(egui::Slider::new(&mut ui.piano_zoom_y, 1.0..=2.5).text("ZoomY"));
        u.add(egui::Slider::new(&mut ui.piano_bot, 21..=72).text("Octave"));
    });
}

fn piano_canvas(
    ui: &mut AppUi,
    s: &mut AppState,
    ch: usize,
    pat: usize,
    u: &mut egui::Ui,
) -> egui::Rect {
    let avail_w = u.available_width().max(300.0);
    let scroll_h = (u.available_height() - 145.0).max(240.0);
    let mut out_rect = egui::Rect::NOTHING;
    egui::ScrollArea::both()
        .id_salt("piano_scroll")
        .max_height(scroll_h)
        .show(u, |u| {
            u.vertical(|u| {
                u.spacing_mut().item_spacing.y = 0.0;
                piano_ruler(ui, s, pat, avail_w, u);
                out_rect = piano_canvas_inner(ui, s, ch, pat, avail_w, u);
            });
        });
    out_rect
}

fn piano_ruler(ui: &mut AppUi, s: &mut AppState, pat: usize, base_avail_w: f32, u: &mut egui::Ui) {
    let pdata = &s.song.patterns[pat];
    let pat_ticks = pdata.len_ticks().max(1);
    let bar_ticks = pdata.time_sig.ticks_per_bar().max(1);
    let beat_ticks = pdata.time_sig.ticks_per_beat().max(1);
    let base_cell_w = (base_avail_w - KEY_W) / pat_ticks as f32;
    let cell_w = base_cell_w * ui.piano_zoom_x;
    let total_w = KEY_W + pat_ticks as f32 * cell_w;
    let (rect, resp) = u.allocate_exact_size(
        egui::vec2(total_w, PIANO_RULER_H),
        egui::Sense::click_and_drag(),
    );
    let p = u.painter_at(rect);
    p.rect_filled(rect, 0.0, crate::ui_widgets::BG_DEEP);
    p.rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(KEY_W, PIANO_RULER_H)),
        0.0,
        crate::ui_widgets::BG_PANEL,
    );
    p.text(
        egui::pos2(rect.min.x + 6.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("P{}", pat + 1),
        egui::FontId::proportional(10.0),
        crate::ui_widgets::TEXT_MUTED,
    );
    for t in 0..pat_ticks {
        let x = rect.min.x + KEY_W + t as f32 * cell_w;
        if t.is_multiple_of(bar_ticks) {
            p.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(1.0, crate::ui_widgets::BORDER_STRONG),
            );
            p.text(
                egui::pos2(x + 4.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                format!("{}", t / bar_ticks + 1),
                egui::FontId::monospace(10.0),
                crate::ui_widgets::TEXT_PRIMARY,
            );
        } else if t.is_multiple_of(beat_ticks) {
            p.line_segment(
                [egui::pos2(x, rect.max.y - 6.0), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(1.0, crate::ui_widgets::BORDER_SUBTLE),
            );
        }
    }
    if s.rt.playing && s.rt.cur_pat == pat {
        let px = rect.min.x + KEY_W + s.rt.cur_tick.min(pat_ticks - 1) as f32 * cell_w;
        p.line_segment(
            [egui::pos2(px, rect.min.y), egui::pos2(px, rect.max.y)],
            egui::Stroke::new(2.0, crate::ui_widgets::ACCENT_GREEN),
        );
    }
    if (resp.clicked() || resp.dragged())
        && let Some(pos) = resp.interact_pointer_pos()
        && pos.x >= rect.min.x + KEY_W
    {
        let raw = ((pos.x - rect.min.x - KEY_W) / cell_w)
            .floor()
            .clamp(0.0, (pat_ticks - 1) as f32) as u32;
        s.seek_tick(raw);
    }
}

fn piano_canvas_inner(
    ui: &mut AppUi,
    s: &mut AppState,
    ch: usize,
    pat: usize,
    base_avail_w: f32,
    u: &mut egui::Ui,
) -> egui::Rect {
    let (midi_lo, midi_hi) = piano_midi_range(ui.piano_bot);
    let rows = (midi_hi - midi_lo + 1) as usize;
    let pdata = &s.song.patterns[pat];
    let pat_ticks = pdata.len_ticks().max(1);
    let bar_ticks = pdata.time_sig.ticks_per_bar().max(1);
    let beat_ticks = pdata.time_sig.ticks_per_beat().max(1);
    let max_tick = (pat_ticks - 1) as i32;
    let base_cell_w = (base_avail_w - KEY_W) / pat_ticks as f32;
    let cell_w = base_cell_w * ui.piano_zoom_x;
    let cell_h = PIANO_CELL_H * ui.piano_zoom_y;
    let total_w = KEY_W + pat_ticks as f32 * cell_w;
    let total_h = rows as f32 * cell_h;
    let (rect, resp) =
        u.allocate_exact_size(egui::vec2(total_w, total_h), egui::Sense::click_and_drag());
    let painter = u.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(16));

    // Middle-mouse pan when zoomed.
    if resp.hovered()
        && u.ctx()
            .input(|i| i.pointer.button_down(egui::PointerButton::Middle))
    {
        let delta = u.ctx().input(|i| i.pointer.delta());
        if delta != egui::Vec2::ZERO {
            u.scroll_with_delta(delta);
        }
    }

    let tick_f_at = |x: f32| (x - rect.min.x - KEY_W) / cell_w;
    let tick_at = |x: f32| tick_f_at(x).floor() as i32;
    let midi_at = |y: f32| midi_hi as i32 - ((y - rect.min.y) / cell_h).floor() as i32;
    let x_of = |tick: u32| rect.min.x + KEY_W + tick as f32 * cell_w;
    let y_of = |midi: u8| rect.min.y + (midi_hi - midi) as f32 * cell_h;

    // Scale shading + key strip.
    let hovered_midi = resp
        .hover_pos()
        .map(|p| midi_at(p.y))
        .filter(|&m| m >= midi_lo as i32 && m <= midi_hi as i32);
    for r in 0..rows {
        let midi = midi_hi - r as u8;
        let y = rect.min.y + r as f32 * cell_h;
        let is_hovered_row = hovered_midi == Some(midi as i32);
        let in_scale = ui.scale.contains(midi % 12, ui.root);
        let row_col = if is_hovered_row {
            egui::Color32::from_rgb(42, 47, 58)
        } else if !in_scale {
            egui::Color32::from_gray(20)
        } else if midi.is_multiple_of(12) {
            egui::Color32::from_rgb(33, 36, 46)
        } else {
            egui::Color32::from_rgb(26, 29, 36)
        };
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(rect.min.x + KEY_W, y),
                egui::vec2(rect.width() - KEY_W, cell_h),
            ),
            0.0,
            row_col,
        );
        painter.line_segment(
            [
                egui::pos2(rect.min.x + KEY_W, y + cell_h),
                egui::pos2(rect.max.x, y + cell_h),
            ],
            egui::Stroke::new(
                1.0,
                if midi.is_multiple_of(12) {
                    crate::ui_widgets::BORDER_STRONG
                } else {
                    egui::Color32::from_gray(32)
                },
            ),
        );
        let black = matches!(midi % 12, 1 | 3 | 6 | 8 | 10);
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(rect.min.x, y), egui::vec2(KEY_W, cell_h)),
            0.0,
            if is_hovered_row {
                if black {
                    egui::Color32::from_rgb(65, 76, 94)
                } else {
                    egui::Color32::from_rgb(232, 240, 250)
                }
            } else if black {
                egui::Color32::from_rgb(26, 29, 36)
            } else {
                egui::Color32::from_rgb(208, 212, 222)
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

    // Grid lines (bar, beat, step + triplet hints when active).
    for t in 0u32..=pat_ticks {
        let x = x_of(t);
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
    if ui.show_ghosts
        && let Some(pdata) = s.song.patterns.get(pat)
    {
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
    ui.sel_notes.retain(|&idx| idx < notes_snapshot.len());
    for (idx, n) in notes_snapshot.iter().enumerate() {
        if n.midi < midi_lo || n.midi > midi_hi || n.tick >= pat_ticks {
            continue;
        }
        let selected = ui.sel_notes.contains(&idx);
        let note_rect = egui::Rect::from_min_max(
            egui::pos2(x_of(n.tick), y_of(n.midi) + 1.0),
            egui::pos2(
                x_of((n.tick + n.len).min(pat_ticks)),
                y_of(n.midi) + cell_h - 1.0,
            ),
        );
        let col = if selected {
            egui::Color32::YELLOW
        } else if n.mute {
            egui::Color32::from_gray(85)
        } else {
            let g = (120.0 + n.vel * 120.0) as u8;
            egui::Color32::from_rgb(60, g, 90)
        };
        painter.rect_filled(note_rect, 2.0, col);
        if n.slide && note_rect.width() >= 5.0 {
            let x1 = note_rect.max.x - 1.5;
            let x0 = (note_rect.max.x - (note_rect.height() * 1.1).min(note_rect.width() * 0.6))
                .max(note_rect.min.x + 1.5);
            let tri = vec![
                egui::pos2(x0, note_rect.max.y - 1.5),
                egui::pos2(x1, note_rect.max.y - 1.5),
                egui::pos2(x1, note_rect.min.y + 1.5),
            ];
            painter.add(egui::Shape::convex_polygon(
                tri,
                if selected {
                    egui::Color32::from_rgb(30, 30, 30)
                } else {
                    egui::Color32::from_rgb(235, 245, 255)
                },
                egui::Stroke::NONE,
            ));
        }
        if n.mute {
            let ym = note_rect.center().y;
            painter.line_segment(
                [
                    egui::pos2(note_rect.min.x + 2.0, ym),
                    egui::pos2(note_rect.max.x - 2.0, ym),
                ],
                egui::Stroke::new(1.5, egui::Color32::from_rgb(200, 90, 90)),
            );
        }
    }

    // Marquee overlay.
    if let Some((a, b)) = ui.marquee {
        let mr = egui::Rect::from_two_pos(a, b);
        painter.rect_filled(
            mr,
            1.0,
            egui::Color32::from_rgba_unmultiplied(120, 190, 255, 35),
        );
        painter.rect_stroke(
            mr,
            1.0,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(130, 210, 255)),
            egui::StrokeKind::Inside,
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

    // Helpers & modifiers.
    let hit_note = |tick: i32, midi: i32| -> Option<usize> {
        notes_snapshot.iter().rposition(|n| {
            tick >= n.tick as i32 && tick < (n.tick + n.len) as i32 && midi == n.midi as i32
        })
    };
    let in_grid = |t: i32, m: i32| {
        (0..pat_ticks as i32).contains(&t) && m >= midi_lo as i32 && m <= midi_hi as i32
    };
    let (origin, mods, scroll_y) = u.ctx().input(|i| {
        (
            i.pointer.press_origin(),
            i.modifiers,
            i.smooth_scroll_delta.y,
        )
    });
    let snap_grid = ui.snap_grid;
    let snap_pos = |x: f32| -> u32 {
        let tf = tick_f_at(x);
        if mods.alt {
            tf.floor().clamp(0.0, max_tick as f32) as u32
        } else {
            snap_grid.snap_floor(tf, cell_w, pat_ticks)
        }
    };

    // Alt + wheel over hovered or selected notes adjusts velocity.
    if resp.hovered() && mods.alt && scroll_y.abs() > 0.5 {
        let delta = if scroll_y > 0.0 { 0.05 } else { -0.05 };
        if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
            for &idx in &ui.sel_notes {
                if let Some(n) = notes.get_mut(idx) {
                    n.vel = (n.vel + delta).clamp(0.05, 1.0);
                }
            }
        }
    }

    if resp.drag_started_by(egui::PointerButton::Secondary)
        && let Some(p0) = origin
    {
        ui.focus = FocusPanel::PianoRoll;
        let t0 = tick_at(p0.x);
        let m0 = midi_at(p0.y);
        if in_grid(t0, m0)
            && let Some(idx) = hit_note(t0, m0)
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
            && idx < notes.len()
        {
            notes.remove(idx);
            ui.sel_notes.clear();
        }
    } else if resp.drag_started()
        && let Some(p0) = origin
    {
        ui.focus = FocusPanel::PianoRoll;
        let t0 = tick_at(p0.x);
        let m0 = midi_at(p0.y);
        if p0.x < rect.min.x + KEY_W {
            if m0 >= midi_lo as i32 && m0 <= midi_hi as i32 {
                s.live_note_ch(ch, m0 as u8);
            }
        } else if in_grid(t0, m0) {
            ui.press_cell = Some((t0, m0));
            ui.paint_last_tick = None;
            if let Some(idx) = hit_note(t0, m0) {
                match ui.tool {
                    PianoTool::Erase => {
                        if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                            && idx < notes.len()
                        {
                            notes.remove(idx);
                            ui.sel_notes.clear();
                        }
                    }
                    PianoTool::Hear => s.live_note_ch(ch, m0 as u8),
                    PianoTool::Slice => {
                        let cut = snap_pos(p0.x);
                        if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                            && tool_slice_at(notes, cut, Some(m0 as u8))
                        {
                            ui.sel_notes.clear();
                        }
                    }
                    PianoTool::Mute => {
                        if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                            && let Some(n) = notes.get_mut(idx)
                        {
                            n.mute = !n.mute;
                        }
                    }
                    PianoTool::Draw | PianoTool::Paint => {
                        if !ui.sel_notes.contains(&idx) && !mods.command {
                            ui.sel_notes = vec![idx];
                        }
                        if mods.shift
                            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                        {
                            // Shift+drag clones selected notes before moving.
                            let mut cloned_idxs = Vec::new();
                            for &si in &ui.sel_notes {
                                if let Some(src) = notes.get(si).copied() {
                                    cloned_idxs.push(notes.len());
                                    notes.push(src);
                                }
                            }
                            if !cloned_idxs.is_empty() {
                                ui.sel_notes = cloned_idxs;
                            }
                        }
                        if let Some(notes) = s.song.patterns[pat].notes.get(ch)
                            && let Some(anchor_idx) = ui.sel_notes.last().copied().or(Some(idx))
                            && let Some(anchor) = notes.get(anchor_idx)
                        {
                            let right_edge = x_of(anchor.tick + anchor.len);
                            let note_px_w = (right_edge - x_of(anchor.tick)).max(1.0);
                            let resize_margin = (note_px_w * 0.35).clamp(3.0, 7.0);
                            let kind = if p0.x >= right_edge - resize_margin {
                                DragKind::Resize
                            } else {
                                DragKind::Move
                            };
                            let orig = ui
                                .sel_notes
                                .iter()
                                .filter_map(|&si| {
                                    notes.get(si).map(|n| (si, n.tick, n.midi, n.len))
                                })
                                .collect();
                            ui.drag = Some(NoteDragState {
                                anchor_idx,
                                kind,
                                start_tick: t0,
                                start_midi: m0,
                                orig,
                            });
                        }
                    }
                }
            } else if (mods.shift || mods.command) && ui.tool == PianoTool::Draw {
                ui.marquee = Some((p0, p0));
            } else if ui.tool == PianoTool::Paint {
                let st = snap_pos(p0.x);
                place_note(ui, s, ch, pat, st, scale_checked(ui, m0 as u8), cell_w);
                ui.paint_last_tick = Some(st);
            }
        }
    }

    if resp.drag_stopped() {
        if let Some((a, b)) = ui.marquee.take() {
            let mr = egui::Rect::from_two_pos(a, b);
            ui.sel_notes = notes_snapshot
                .iter()
                .enumerate()
                .filter_map(|(idx, n)| {
                    if n.midi < midi_lo || n.midi > midi_hi || n.tick >= pat_ticks {
                        return None;
                    }
                    let nr = egui::Rect::from_min_max(
                        egui::pos2(x_of(n.tick), y_of(n.midi) + 1.0),
                        egui::pos2(
                            x_of((n.tick + n.len).min(pat_ticks)),
                            y_of(n.midi) + cell_h - 1.0,
                        ),
                    );
                    mr.intersects(nr).then_some(idx)
                })
                .collect();
            ui.status = format!("selected {} notes", ui.sel_notes.len());
        } else if ui.drag.is_none()
            && ui.tool == PianoTool::Draw
            && let Some((t0, m0)) = ui.press_cell
            && in_grid(t0, m0)
        {
            let snap_t = if mods.alt {
                t0.clamp(0, max_tick) as u32
            } else {
                ui.snap_grid.snap_floor(t0 as f32, cell_w, pat_ticks)
            };
            place_note(ui, s, ch, pat, snap_t, scale_checked(ui, m0 as u8), cell_w);
        }
        ui.drag = None;
        ui.press_cell = None;
        ui.paint_last_tick = None;
    } else if resp.dragged() {
        if let Some((a, _)) = ui.marquee
            && let Some(pos) = resp.hover_pos()
        {
            ui.marquee = Some((a, pos));
        } else if let Some(d) = &ui.drag
            && let Some(pos) = resp.hover_pos()
        {
            let t = tick_at(pos.x);
            let m = midi_at(pos.y);
            let dt = t - d.start_tick;
            let dm = m - d.start_midi;
            if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
                for &(si, ot, om, ol) in &d.orig {
                    if let Some(n) = notes.get_mut(si) {
                        match d.kind {
                            DragKind::Move => {
                                n.tick = (ot as i32 + dt).clamp(0, max_tick) as u32;
                                let target = (om as i32 + dm).clamp(21, 108) as u8;
                                n.midi = scale_checked(ui, target);
                            }
                            DragKind::Resize => {
                                n.len = (ol as i32 + dt).clamp(1, pat_ticks as i32) as u32;
                            }
                        }
                    }
                }
                let _ = d.anchor_idx;
            }
        } else if ui.tool == PianoTool::Paint
            && let Some(pos) = resp.hover_pos()
        {
            let t = tick_at(pos.x);
            let m = midi_at(pos.y);
            if in_grid(t, m) {
                let st = snap_pos(pos.x);
                if ui.paint_last_tick != Some(st) && hit_note(st as i32, m).is_none() {
                    place_note(ui, s, ch, pat, st, scale_checked(ui, m as u8), cell_w);
                    ui.paint_last_tick = Some(st);
                }
            }
        } else if ui.tool == PianoTool::Slice
            && let Some(pos) = resp.hover_pos()
        {
            let t = tick_at(pos.x);
            let m = midi_at(pos.y);
            if in_grid(t, m) && hit_note(t, m).is_some() {
                let cut = snap_pos(pos.x);
                if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch) {
                    tool_slice_at(notes, cut, Some(m as u8));
                }
            }
        } else if (ui.tool == PianoTool::Erase || resp.dragged_by(egui::PointerButton::Secondary))
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
                ui.sel_notes.clear();
            }
        }
    }

    if resp.clicked()
        && !resp.drag_stopped()
        && let Some(pos) = resp.hover_pos()
    {
        ui.focus = FocusPanel::PianoRoll;
        let t = tick_at(pos.x);
        let m = midi_at(pos.y);
        if pos.x < rect.min.x + KEY_W {
            if m >= midi_lo as i32 && m <= midi_hi as i32 {
                s.live_note_ch(ch, m as u8);
            }
        } else if in_grid(t, m) {
            match ui.tool {
                PianoTool::Draw | PianoTool::Paint => {
                    if let Some(idx) = hit_note(t, m) {
                        if mods.command {
                            if let Some(pos) = ui.sel_notes.iter().position(|&x| x == idx) {
                                ui.sel_notes.remove(pos);
                            } else {
                                ui.sel_notes.push(idx);
                            }
                        } else {
                            ui.sel_notes = vec![idx];
                        }
                    } else {
                        let snap_t = snap_pos(pos.x);
                        let mp = scale_checked(ui, m as u8);
                        place_note(ui, s, ch, pat, snap_t, mp, cell_w);
                    }
                }
                PianoTool::Slice => {
                    let cut = if mods.alt {
                        tick_f_at(pos.x).round().clamp(0.0, max_tick as f32) as u32
                    } else {
                        snap_grid.snap_nearest(tick_f_at(pos.x), cell_w, pat_ticks)
                    };
                    if let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                        && tool_slice_at(notes, cut, Some(m as u8))
                    {
                        ui.sel_notes.clear();
                        ui.status = format!("sliced at tick {cut}");
                    }
                }
                PianoTool::Mute => {
                    if let Some(idx) = hit_note(t, m)
                        && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                        && let Some(n) = notes.get_mut(idx)
                    {
                        n.mute = !n.mute;
                        ui.status = if n.mute {
                            "muted note".into()
                        } else {
                            "unmuted note".into()
                        };
                    }
                }
                PianoTool::Erase => {
                    if let Some(idx) = hit_note(t, m)
                        && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
                        && idx < notes.len()
                    {
                        notes.remove(idx);
                        ui.sel_notes.clear();
                    }
                }
                PianoTool::Hear => s.live_note_ch(ch, m as u8),
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
            ui.sel_notes.clear();
        }
    }

    if resp.double_clicked()
        && let Some(pos) = resp.hover_pos()
    {
        let t = tick_at(pos.x);
        let m = midi_at(pos.y);
        if in_grid(t, m) {
            if let Some(idx) = hit_note(t, m) {
                ui.prop_note = Some(idx);
                ui.sel_notes = vec![idx];
            } else if ui.show_ghosts
                && ui.editable_ghosts
                && let Some(pdata) = s.song.patterns.get(pat)
            {
                for (ci, notes) in pdata.notes.iter().enumerate() {
                    if ci != ch
                        && let Some(gi) = notes.iter().position(|n| {
                            t >= n.tick as i32 && t < (n.tick + n.len) as i32 && m == n.midi as i32
                        })
                    {
                        ui.sel_ch = ci;
                        ui.sel_notes = vec![gi];
                        ui.status = format!("switched to {}", s.song.channels[ci].name);
                        break;
                    }
                }
            }
        }
    }

    // Keyboard shortcuts while Piano Roll is focused or hovered.
    if (ui.focus == FocusPanel::PianoRoll || resp.hovered()) && !u.ctx().egui_wants_keyboard_input()
    {
        let (del, up, down, shift, alt_g) = u.ctx().input(|i| {
            (
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.modifiers.shift,
                i.modifiers.alt && i.key_pressed(egui::Key::G),
            )
        });
        if alt_g {
            ui.show_ghosts = !ui.show_ghosts;
        }
        if del {
            delete_selected_notes(ui, s);
        }
        if (up || down)
            && !ui.sel_notes.is_empty()
            && let Some(notes) = s.song.patterns[pat].notes.get_mut(ch)
        {
            let step = if shift { 12i32 } else { 1i32 } * if up { 1 } else { -1 };
            for &si in &ui.sel_notes {
                if let Some(n) = notes.get_mut(si) {
                    n.midi = (n.midi as i32 + step).clamp(21, 108) as u8;
                }
            }
        }
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
                        w.horizontal(|w| {
                            w.checkbox(&mut n.slide, "Slide");
                            w.checkbox(&mut n.mute, "Mute");
                        });
                        w.add(egui::Slider::new(&mut n.vel, 0.05..=1.0).text("Velocity"));
                        w.add(egui::Slider::new(&mut n.pan, -1.0..=1.0).text("Pan"));
                        w.add(
                            egui::Slider::new(&mut n.fine_pitch, -12.0..=12.0).text("Fine pitch"),
                        );
                        w.add(egui::Slider::new(&mut n.cutoff, 0.0..=1.0).text("Cutoff"));
                        w.add(egui::Slider::new(&mut n.release_bias, 0.0..=1.0).text("Release"));
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
                ui.sel_notes.clear();
            }
            if remove || close {
                ui.prop_note = None;
            }
        }
    }
}

fn piano_tool_modal(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    let Some(mut modal) = ui.modal.take() else {
        return;
    };
    let title = match modal.kind {
        PianoModalKind::Quantize => "Quantizer",
        PianoModalKind::Strum => "Strumizer",
        PianoModalKind::Flam => "Flam",
        PianoModalKind::Arp => "Arpeggiator",
        PianoModalKind::Randomize => "Randomizer",
        PianoModalKind::Flip => "Flip",
    };
    let mut accept = false;
    let mut cancel = false;
    let mut changed = false;
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .show(u.ctx(), |w| {
            match modal.kind {
                PianoModalKind::Quantize => {
                    changed |= w
                        .add(
                            egui::Slider::new(&mut modal.quant.grid, 1.0..=16.0).text("Grid ticks"),
                        )
                        .changed();
                    changed |= w
                        .add(
                            egui::Slider::new(&mut modal.quant.strength, 0.0..=1.0)
                                .text("Strength"),
                        )
                        .changed();
                    changed |= w
                        .add(egui::Slider::new(&mut modal.quant.swing, 0.0..=0.75).text("Swing"))
                        .changed();
                }
                PianoModalKind::Strum => {
                    changed |= w
                        .add(
                            egui::DragValue::new(&mut modal.strum.amount)
                                .range(0..=16)
                                .prefix("Time: "),
                        )
                        .changed();
                    changed |= w
                        .add(
                            egui::Slider::new(&mut modal.strum.vel_slope, -0.5..=0.5)
                                .text("Vel tilt"),
                        )
                        .changed();
                    changed |= w
                        .checkbox(&mut modal.strum.preserve_end, "Preserve chord end")
                        .changed();
                }
                PianoModalKind::Flam => {
                    changed |= w
                        .add(
                            egui::DragValue::new(&mut modal.flam_gap)
                                .range(1..=12)
                                .prefix("Gap: "),
                        )
                        .changed();
                    changed |= w
                        .add(egui::Slider::new(&mut modal.flam_vel, 0.1..=1.0).text("Grace vel"))
                        .changed();
                }
                PianoModalKind::Arp => {
                    changed |= w
                        .add(
                            egui::DragValue::new(&mut modal.arp.step_ticks)
                                .range(1..=16)
                                .prefix("Step: "),
                        )
                        .changed();
                    changed |= w
                        .add(egui::Slider::new(&mut modal.arp.gate, 0.1..=1.0).text("Gate"))
                        .changed();
                    changed |= w
                        .add(
                            egui::DragValue::new(&mut modal.arp.octaves)
                                .range(1..=4)
                                .prefix("Octaves: "),
                        )
                        .changed();
                    w.horizontal(|w| {
                        for (d, label) in [
                            (ArpDir::Up, "Up"),
                            (ArpDir::Down, "Down"),
                            (ArpDir::UpDown, "UpDown"),
                        ] {
                            changed |= w.selectable_value(&mut modal.arp.dir, d, label).changed();
                        }
                    });
                }
                PianoModalKind::Randomize => {
                    changed |= w
                        .add(
                            egui::DragValue::new(&mut modal.rand.pitch_range)
                                .range(0..=12)
                                .prefix("Pitch st: "),
                        )
                        .changed();
                    changed |= w
                        .add(egui::Slider::new(&mut modal.rand.vel_amt, 0.0..=0.8).text("Vel amt"))
                        .changed();
                    changed |= w
                        .add(egui::Slider::new(&mut modal.rand.pan_amt, 0.0..=1.0).text("Pan amt"))
                        .changed();
                    changed |= w
                        .add(
                            egui::DragValue::new(&mut modal.rand.seed)
                                .range(1..=9999)
                                .prefix("Seed: "),
                        )
                        .changed();
                }
                PianoModalKind::Flip => {
                    changed |= w.checkbox(&mut modal.flip_h, "Flip horizontal").changed();
                    changed |= w.checkbox(&mut modal.flip_v, "Flip vertical").changed();
                }
            }
            w.horizontal(|w| {
                if w.button("Accept").clicked() {
                    accept = true;
                }
                if w.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });

    if cancel {
        if let Some(notes) = s
            .song
            .patterns
            .get_mut(modal.pat)
            .and_then(|p| p.notes.get_mut(modal.ch))
        {
            *notes = modal.backup;
        }
        ui.status = format!("{title} cancelled");
    } else {
        if changed {
            apply_piano_modal_preview(s, &modal);
        }
        if accept {
            ui.status = format!("{title} applied");
        } else {
            ui.modal = Some(modal);
        }
    }
}

fn note_property_lane(ui: &mut AppUi, s: &mut AppState, ch: usize, pat: usize, u: &mut egui::Ui) {
    u.horizontal(|u| {
        u.label("Note lane:");
        for lane in NoteLane::ALL {
            u.selectable_value(&mut ui.note_lane, lane, lane.name());
        }
        u.label("(drag bars)");
    });
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
    for (idx, n) in notes.iter().enumerate() {
        if n.tick >= pat_ticks {
            continue;
        }
        let x =
            rect.min.x + KEY_W + (n.tick as f32 + 0.5) / pat_ticks as f32 * (rect.width() - KEY_W);
        let (norm, col) = match ui.note_lane {
            NoteLane::Velocity => (n.vel.clamp(0.0, 1.0), egui::Color32::from_rgb(90, 160, 120)),
            NoteLane::Pan => (
                (n.pan.clamp(-1.0, 1.0) + 1.0) * 0.5,
                egui::Color32::from_rgb(100, 165, 220),
            ),
            NoteLane::Cutoff => (
                n.cutoff.clamp(0.0, 1.0),
                egui::Color32::from_rgb(200, 145, 85),
            ),
            NoteLane::FinePitch => (
                (n.fine_pitch.clamp(-12.0, 12.0) + 12.0) / 24.0,
                egui::Color32::from_rgb(180, 120, 220),
            ),
            NoteLane::Release => (
                n.release_bias.clamp(0.0, 1.0),
                egui::Color32::from_rgb(140, 190, 95),
            ),
        };
        let col = if ui.sel_notes.contains(&idx) {
            egui::Color32::YELLOW
        } else {
            col
        };
        let bh = norm * (h - 6.0);
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(x - 3.0, rect.max.y - 3.0 - bh),
                egui::vec2(6.0, bh),
            ),
            1.0,
            col,
        );
    }
    if (resp.dragged() || resp.clicked())
        && let Some(pos) = resp.hover_pos()
    {
        let tick = (((pos.x - rect.min.x - KEY_W) / (rect.width() - KEY_W) * pat_ticks as f32)
            .round() as i32)
            .clamp(0, pat_ticks as i32 - 1);
        let norm = (1.0 - (pos.y - rect.min.y) / h).clamp(0.0, 1.0);
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
                match ui.note_lane {
                    NoteLane::Velocity => notes[idx].vel = norm.clamp(0.05, 1.0),
                    NoteLane::Pan => notes[idx].pan = norm * 2.0 - 1.0,
                    NoteLane::Cutoff => notes[idx].cutoff = norm,
                    NoteLane::FinePitch => notes[idx].fine_pitch = (norm * 24.0 - 12.0).round(),
                    NoteLane::Release => notes[idx].release_bias = norm,
                }
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
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .collapsible(false)
        .resizable(false)
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
            sync_dock_tabs(ui);
            focus_dock_tab(&mut ui.dock_state, DockTab::PianoRoll);
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

        if let Some(payload) = resp.dnd_release_payload::<BrowserDragPayload>()
            && let Some(pos) = u.ctx().pointer_latest_pos()
            && lane_rect.contains(pos)
        {
            let tr = ((pos.y - lane_rect.min.y) / TRACK_ROW_H)
                .floor()
                .clamp(0.0, (ntracks.saturating_sub(1)) as f32) as usize;
            let raw_t = ((pos.x - lane_rect.min.x) / tick_w).max(0.0) as u32;
            let snapped = (raw_t / snap_ticks) * snap_ticks;
            drop_browser_onto_playlist(ui, s, tr, snapped, &payload);
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
    let sel_idx = ui
        .playlist_auto_pt
        .filter(|&i| i < points.len())
        .or_else(|| (!points.is_empty()).then_some(0));
    u.horizontal_wrapped(|u| {
        u.label(format!("Automation clip ({})", target.label()));
        if let Some(si) = sel_idx
            && let Some(pt) = points.get_mut(si)
        {
            u.label(format!("Pt {}:", si + 1));
            egui::ComboBox::from_id_salt(("auto_pt_curve", cid))
                .width(76.0)
                .selected_text(pt.curve.name())
                .show_ui(u, |u| {
                    for cm in CurveMode::ALL {
                        u.selectable_value(&mut pt.curve, cm, cm.name());
                    }
                });
            u.add(egui::Slider::new(&mut pt.tension, -1.0..=1.0).text("Tension"));
        }
        u.label("(click add/select · drag move · right-click remove)");
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
    for (pi, pt) in points.iter().enumerate() {
        let center = egui::pos2(x_of(pt.tick), y_of(pt.value));
        if sel_idx == Some(pi) {
            painter.circle_stroke(
                center,
                6.5,
                egui::Stroke::new(1.5, egui::Color32::LIGHT_GREEN),
            );
        }
        painter.circle_filled(center, 4.0, egui::Color32::YELLOW);
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
            ui.playlist_auto_pt = Some(idx);
        } else {
            points.push(AutoClipPoint {
                tick: tick_at(p0.x),
                value: val_at(p0.y),
                tension: 0.0,
                curve: CurveMode::Linear,
            });
            points.sort_by_key(|p| p.tick);
            if let Some(idx) = near_idx(points, p0) {
                ui.playlist_auto_drag = Some((cid, idx));
                ui.playlist_auto_pt = Some(idx);
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
        ui.playlist_auto_pt = near_idx(points, pos);
    }
    if resp.clicked()
        && !resp.drag_stopped()
        && let Some(pos) = resp.hover_pos()
    {
        if let Some(idx) = near_idx(points, pos) {
            ui.playlist_auto_pt = Some(idx);
        } else {
            points.push(AutoClipPoint {
                tick: tick_at(pos.x),
                value: val_at(pos.y),
                tension: 0.0,
                curve: CurveMode::Linear,
            });
            points.sort_by_key(|p| p.tick);
            ui.playlist_auto_pt = near_idx(points, pos);
        }
    }
    if resp.clicked_by(egui::PointerButton::Secondary)
        && let Some(pos) = resp.hover_pos()
        && points.len() > 2
        && let Some(idx) = near_idx(points, pos)
    {
        points.remove(idx);
        ui.playlist_auto_pt = ui
            .playlist_auto_pt
            .map(|i| i.min(points.len().saturating_sub(1)));
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

pub fn fx_kind_from_choice(choice: usize) -> FxKind {
    match choice {
        0 => FxKind::ParametricEq(EqCfg::default()),
        1 => FxKind::Compressor(CompCfg::default()),
        2 => FxKind::Compressor(CompCfg {
            threshold_db: -1.5,
            ratio: 20.0,
            knee_db: 0.5,
            attack_ms: 0.5,
            release_ms: 45.0,
            makeup_db: 0.0,
            sidechain_from: None,
            limiter: true,
            lookahead_samples: 64,
        }),
        3 => FxKind::StereoDelay(DelayFxCfg::default()),
        4 => FxKind::Reverb(ReverbFxCfg::default()),
        5 => FxKind::Modulator(ModFxCfg::default()),
        6 => FxKind::Saturator(SatCfg::default()),
        _ => FxKind::StereoShaper(StereoShaperCfg::default()),
    }
}

const FX_CHOICE_NAMES: [&str; 8] = [
    "Parametric EQ",
    "Compressor",
    "Limiter",
    "Stereo Delay",
    "Reverb",
    "Modulator",
    "Saturator",
    "Stereo Shaper",
];

struct FxEditorCtx<'a> {
    slot_idx: usize,
    ins: usize,
    names: &'a [String],
    sr: f32,
    fft_bins: &'a [f32; fx::FFT_BINS],
    gr_db: f32,
    clap: &'a mut ClapManager,
    parent: Option<RawWindowHandle>,
    clap_filter: &'a mut String,
    clap_out: &'a mut Option<(u64, ClapUiOutput)>,
}

fn fx_slot_editor(
    slot: &mut EffectSlot,
    ctx: &mut FxEditorCtx<'_>,
    u: &mut egui::Ui,
) -> Option<ParamCtxAction> {
    let slot_idx = ctx.slot_idx;
    let ins = ctx.ins;
    let names = ctx.names;
    let sr = ctx.sr;
    let fft_bins = ctx.fft_bins;
    let gr_db = ctx.gr_db;
    let mut act = None;
    u.horizontal(|u| {
        u.label(format!("Slot {}: {}", slot_idx + 1, slot.name));
        u.toggle_value(&mut slot.bypass, "Bypass");
        let r = u.add(egui::Slider::new(&mut slot.mix, 0.0..=1.0).text("Mix"));
        collect_param_ctx(&r, AutoTarget::InsertFxMix(ins, slot_idx), &mut act);
    });
    match &mut slot.kind {
        FxKind::ParametricEq(cfg) => {
            // Visual 64-bin FFT + combined EQ response curve.
            let (rect, _) = u.allocate_exact_size(
                egui::vec2(u.available_width().max(240.0), 86.0),
                egui::Sense::hover(),
            );
            let p = u.painter_at(rect);
            p.rect_filled(rect, 3.0, egui::Color32::from_gray(20));
            let mid_y = rect.center().y;
            p.line_segment(
                [egui::pos2(rect.min.x, mid_y), egui::pos2(rect.max.x, mid_y)],
                egui::Stroke::new(1.0, egui::Color32::from_gray(50)),
            );
            // FFT bars (dB values in -72.0..=6.0)
            let bw = rect.width() / fx::FFT_BINS as f32;
            for (bi, &db) in fft_bins.iter().enumerate() {
                let norm = ((db + 72.0) / 72.0).clamp(0.0, 1.0);
                if norm > 0.01 {
                    let h = norm * rect.height() * 0.9;
                    let x = rect.min.x + bi as f32 * bw;
                    p.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(x, rect.max.y - h),
                            egui::vec2((bw - 1.0).max(1.0), h),
                        ),
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(70, 155, 220, 110),
                    );
                }
            }
            // EQ curve (20 Hz .. 20 kHz log scale)
            let steps = 48;
            let mut prev: Option<egui::Pos2> = None;
            for k in 0..=steps {
                let t = k as f32 / steps as f32;
                let hz = 20.0 * (1000.0_f32).powf(t);
                let db = fx::eq_response_db(cfg, hz, sr).clamp(-18.0, 18.0);
                let x = rect.min.x + t * rect.width();
                let y = mid_y - (db / 18.0) * (rect.height() * 0.44);
                let pt = egui::pos2(x, y);
                if let Some(pp) = prev {
                    p.line_segment([pp, pt], egui::Stroke::new(1.8, egui::Color32::GOLD));
                }
                prev = Some(pt);
            }
            for (bi, band) in cfg.bands.iter_mut().enumerate() {
                u.horizontal(|u| {
                    u.toggle_value(&mut band.enabled, format!("B{}", bi + 1));
                    egui::ComboBox::from_id_salt(("eq_band_kind", ins, slot_idx, bi))
                        .width(68.0)
                        .selected_text(band.kind.name())
                        .show_ui(u, |u| {
                            for k in EqBandType::ALL {
                                u.selectable_value(&mut band.kind, k, k.name());
                            }
                        });
                    u.add(
                        egui::Slider::new(&mut band.freq_hz, 20.0..=18000.0)
                            .logarithmic(true)
                            .suffix(" Hz"),
                    );
                    u.add(egui::Slider::new(&mut band.gain_db, -18.0..=18.0).suffix(" dB"));
                    u.add(egui::Slider::new(&mut band.q, 0.3..=10.0).text("Q"));
                });
            }
        }
        FxKind::Compressor(cfg) => {
            u.horizontal(|u| {
                u.toggle_value(&mut cfg.limiter, "Limiter");
                let sc_label = cfg
                    .sidechain_from
                    .and_then(|idx| names.get(idx).cloned())
                    .unwrap_or_else(|| "Internal".into());
                egui::ComboBox::from_id_salt(("comp_sc", ins, slot_idx))
                    .selected_text(format!("SC: {sc_label}"))
                    .show_ui(u, |u| {
                        u.selectable_value(&mut cfg.sidechain_from, None, "Internal");
                        for (mi, nm) in names.iter().enumerate() {
                            if mi != ins {
                                u.selectable_value(&mut cfg.sidechain_from, Some(mi), nm.clone());
                            }
                        }
                    });
                u.label(format!("GR: {:.1} dB", gr_db));
                let gr_norm = (-gr_db / 24.0).clamp(0.0, 1.0);
                let (grect, _) =
                    u.allocate_exact_size(egui::vec2(72.0, 10.0), egui::Sense::hover());
                let gp = u.painter_at(grect);
                gp.rect_filled(grect, 2.0, egui::Color32::from_gray(28));
                if gr_norm > 0.005 {
                    let w = grect.width() * gr_norm;
                    gp.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(grect.max.x - w, grect.min.y),
                            egui::vec2(w, grect.height()),
                        ),
                        1.0,
                        egui::Color32::from_rgb(230, 145, 55),
                    );
                }
            });
            u.horizontal(|u| {
                u.add(
                    egui::Slider::new(&mut cfg.threshold_db, -48.0..=0.0)
                        .text("Thresh")
                        .suffix(" dB"),
                );
                u.add(egui::Slider::new(&mut cfg.ratio, 1.0..=20.0).text("Ratio"));
                u.add(
                    egui::Slider::new(&mut cfg.knee_db, 0.0..=12.0)
                        .text("Knee")
                        .suffix(" dB"),
                );
            });
            u.horizontal(|u| {
                u.add(
                    egui::Slider::new(&mut cfg.attack_ms, 0.2..=80.0)
                        .text("Atk")
                        .suffix(" ms"),
                );
                u.add(
                    egui::Slider::new(&mut cfg.release_ms, 10.0..=600.0)
                        .text("Rel")
                        .suffix(" ms"),
                );
                u.add(
                    egui::Slider::new(&mut cfg.makeup_db, -12.0..=18.0)
                        .text("Gain")
                        .suffix(" dB"),
                );
            });
            if cfg.limiter {
                u.add(
                    egui::Slider::new(&mut cfg.lookahead_samples, 0..=256)
                        .text("Lookahead")
                        .suffix(" smp"),
                );
            }
        }
        FxKind::StereoDelay(cfg) => {
            u.horizontal(|u| {
                egui::ComboBox::from_id_salt(("dly_sync", ins, slot_idx))
                    .selected_text(cfg.sync.name())
                    .show_ui(u, |u| {
                        for sy in DelaySync::ALL {
                            u.selectable_value(&mut cfg.sync, sy, sy.name());
                        }
                    });
                u.toggle_value(&mut cfg.ping_pong, "Ping-Pong");
                if cfg.sync == DelaySync::FreeMs {
                    u.add(
                        egui::Slider::new(&mut cfg.time_ms, 10.0..=1500.0)
                            .text("Time")
                            .suffix(" ms"),
                    );
                }
            });
            u.horizontal(|u| {
                u.add(egui::Slider::new(&mut cfg.feedback, 0.0..=0.92).text("Feedback"));
                u.add(
                    egui::Slider::new(&mut cfg.hp_hz, 20.0..=2000.0)
                        .logarithmic(true)
                        .text("HP"),
                );
                u.add(
                    egui::Slider::new(&mut cfg.lp_hz, 800.0..=16000.0)
                        .logarithmic(true)
                        .text("LP"),
                );
            });
        }
        FxKind::Reverb(cfg) => {
            u.horizontal(|u| {
                u.add(egui::Slider::new(&mut cfg.room_size, 0.1..=1.0).text("Room"));
                u.add(egui::Slider::new(&mut cfg.decay, 0.1..=0.95).text("Decay"));
                u.add(
                    egui::Slider::new(&mut cfg.pre_delay_ms, 0.0..=120.0)
                        .text("Pre")
                        .suffix(" ms"),
                );
            });
            u.horizontal(|u| {
                u.add(
                    egui::Slider::new(&mut cfg.high_cut_hz, 1200.0..=16000.0)
                        .logarithmic(true)
                        .text("HiCut"),
                );
                u.add(egui::Slider::new(&mut cfg.width, 0.0..=1.0).text("Width"));
            });
        }
        FxKind::Modulator(cfg) => {
            u.horizontal(|u| {
                egui::ComboBox::from_id_salt(("mod_kind", ins, slot_idx))
                    .selected_text(cfg.mode.name())
                    .show_ui(u, |u| {
                        for m in ModFxKind::ALL {
                            u.selectable_value(&mut cfg.mode, m, m.name());
                        }
                    });
                u.add(
                    egui::Slider::new(&mut cfg.rate_hz, 0.05..=8.0)
                        .text("Rate")
                        .suffix(" Hz"),
                );
                u.add(egui::Slider::new(&mut cfg.depth, 0.0..=1.0).text("Depth"));
                u.add(egui::Slider::new(&mut cfg.feedback, -0.85..=0.85).text("Fb"));
            });
        }
        FxKind::Saturator(cfg) => {
            u.horizontal(|u| {
                egui::ComboBox::from_id_salt(("sat_kind", ins, slot_idx))
                    .selected_text(cfg.mode.name())
                    .show_ui(u, |u| {
                        for sk in SatKind::ALL {
                            u.selectable_value(&mut cfg.mode, sk, sk.name());
                        }
                    });
                u.add(egui::Slider::new(&mut cfg.drive, 1.0..=12.0).text("Drive"));
                u.add(egui::Slider::new(&mut cfg.tone, 0.0..=1.0).text("Tone"));
                u.add(egui::Slider::new(&mut cfg.output_db, -18.0..=6.0).text("Out dB"));
            });
        }
        FxKind::StereoShaper(cfg) => {
            u.horizontal(|u| {
                u.add(egui::Slider::new(&mut cfg.mid_gain, 0.0..=2.0).text("Mid"));
                u.add(egui::Slider::new(&mut cfg.side_gain, 0.0..=2.0).text("Side"));
                u.add(
                    egui::Slider::new(&mut cfg.delay_ms, -20.0..=20.0)
                        .text("L/R dly")
                        .suffix(" ms"),
                );
                u.toggle_value(&mut cfg.invert_l, "Inv L");
                u.toggle_value(&mut cfg.invert_r, "Inv R");
            });
        }
        FxKind::ClapPlugin(cfg) => {
            let out = render_clap_editor_ui(cfg, ctx.clap, ctx.parent, ctx.clap_filter, ctx.sr, u);
            *ctx.clap_out = Some((cfg.rt_id, out));
        }
    }
    act
}

pub fn fx_rack_panel(
    ui: &mut AppUi,
    s: &mut AppState,
    ins: usize,
    names: &[String],
    u: &mut egui::Ui,
) {
    let sr = s.rt.sr;
    let fft_bins = s.rt.fft_bins;
    let comp_gr = s.rt.comp_gr_db;
    let mut ctx_act: Option<ParamCtxAction> = None;
    let mut clap_out: Option<(u64, ClapUiOutput)> = None;
    let mut pick_clap_fx: Option<ClapPluginDesc> = None;
    let mut scan_clap_fx = false;
    let mut browse_clap_fx = false;
    let mut removed_fx = false;
    let Some(track) = s.song.mixer_tracks.get_mut(ins) else {
        return;
    };
    let fx_drop = u.group(|u| {
        u.horizontal_wrapped(|u| {
            u.strong(format!(
                "Insert FX Rack ({}/{})",
                track.fx.len(),
                MAX_FX_SLOTS
            ));
            let choice = ui.add_fx_kind.min(FX_CHOICE_NAMES.len() - 1);
            egui::ComboBox::from_id_salt("add_fx_combo")
                .selected_text(FX_CHOICE_NAMES[choice])
                .show_ui(u, |u| {
                    for (ci, &label) in FX_CHOICE_NAMES.iter().enumerate() {
                        u.selectable_value(&mut ui.add_fx_kind, ci, label);
                    }
                });
            let can_add = track.fx.len() < MAX_FX_SLOTS;
            if u.add_enabled(can_add, egui::Button::new("+ FX")).clicked() {
                let kind = fx_kind_from_choice(ui.add_fx_kind);
                let nm = kind.default_name();
                if track.add_fx(EffectSlot::new(kind)) {
                    ui.sel_fx_slot = track.fx.len() - 1;
                    ui.status = format!("added {nm} to {}", track.name);
                }
            }
            if can_add {
                egui::ComboBox::from_id_salt("add_clap_fx_combo")
                    .selected_text("+ CLAP FX")
                    .width(86.0)
                    .show_ui(u, |u| {
                        let mut shown = 0usize;
                        for desc in ui.clap_catalog.plugins.iter().filter(|d| d.is_effect) {
                            if u.button(&desc.name).clicked() {
                                pick_clap_fx = Some(desc.clone());
                            }
                            shown += 1;
                        }
                        if shown == 0 {
                            for desc in &ui.clap_catalog.plugins {
                                if u.button(&desc.name).clicked() {
                                    pick_clap_fx = Some(desc.clone());
                                }
                            }
                        }
                        if !ui.clap_catalog.plugins.is_empty() {
                            u.separator();
                        }
                        if u.button("Scan CLAP folders").clicked() {
                            scan_clap_fx = true;
                        }
                        if u.button("Load .clap file...").clicked() {
                            browse_clap_fx = true;
                        }
                    });
            }
        });
        if track.fx.is_empty() {
            u.label("No insert effects loaded. Pick a module, drag from Browser, or click + FX.");
            return;
        }
        ui.sel_fx_slot = ui.sel_fx_slot.min(track.fx.len() - 1);
        u.columns(2, |cols| {
            let mut move_op: Option<(usize, usize)> = None;
            let mut rm_idx: Option<usize> = None;
            let len = track.fx.len();
            cols[0].group(|u| {
                for (si, slot) in track.fx.iter_mut().enumerate().take(len) {
                    let selected = ui.sel_fx_slot == si;
                    u.horizontal(|u| {
                        let label = format!("{}. {}", si + 1, slot.name);
                        if u.selectable_label(selected, label).clicked() {
                            ui.sel_fx_slot = si;
                        }
                        u.toggle_value(&mut slot.bypass, "Byp");
                        let mix_r =
                            u.add(egui::Slider::new(&mut slot.mix, 0.0..=1.0).show_value(false));
                        collect_param_ctx(&mix_r, AutoTarget::InsertFxMix(ins, si), &mut ctx_act);
                        if si > 0 && u.small_button("^").clicked() {
                            move_op = Some((si, si - 1));
                        }
                        if si + 1 < len && u.small_button("v").clicked() {
                            move_op = Some((si, si + 1));
                        }
                        if u.small_button("×").clicked() {
                            rm_idx = Some(si);
                        }
                    });
                }
            });
            if let Some((from, to)) = move_op
                && track.move_fx(from, to)
            {
                ui.sel_fx_slot = to;
            }
            if let Some(rm) = rm_idx {
                track.fx.remove(rm);
                removed_fx = true;
                if track.fx.is_empty() {
                    ui.sel_fx_slot = 0;
                    return;
                }
                ui.sel_fx_slot = ui.sel_fx_slot.min(track.fx.len() - 1);
            }
            let sel = ui.sel_fx_slot;
            if let Some(slot) = track.fx.get_mut(sel) {
                let mut ctx = FxEditorCtx {
                    slot_idx: sel,
                    ins,
                    names,
                    sr,
                    fft_bins: &fft_bins,
                    gr_db: comp_gr[sel],
                    clap: &mut ui.clap,
                    parent: ui.parent_window,
                    clap_filter: &mut ui.clap_param_filter,
                    clap_out: &mut clap_out,
                };
                cols[1].group(|u| {
                    if let Some(a) = fx_slot_editor(slot, &mut ctx, u) {
                        ctx_act = Some(a);
                    }
                });
            }
        });
    });
    if removed_fx {
        sync_clap_plugins(ui, s);
    }
    if scan_clap_fx {
        scan_all_clap_plugins(ui);
    }
    if browse_clap_fx
        && let Some(p) = rfd::FileDialog::new()
            .add_filter("CLAP Plugin", &["clap"])
            .pick_file()
    {
        match register_clap_file_into_catalog(ui, &p) {
            Ok(descs) => {
                if let Some(d) = descs.iter().find(|d| d.is_effect).or_else(|| descs.first()) {
                    add_clap_fx_from_desc(ui, s, ins, d);
                } else {
                    ui.status = "no CLAP plugins in selected file".into();
                }
            }
            Err(e) => ui.status = format!("CLAP scan failed: {e}"),
        }
    }
    if let Some(desc) = pick_clap_fx {
        add_clap_fx_from_desc(ui, s, ins, &desc);
    }
    if let Some((rt_id, co)) = clap_out {
        if let Some(msg) = co.status {
            ui.status = msg;
        }
        if let Some(node) = co.reloaded {
            s.mount_clap(rt_id, node);
        }
        for (pid, val) in co.param_updates {
            s.set_clap_param(rt_id, pid, val);
        }
        if co.graph_dirty {
            s.rt.ensure_channels(&s.song);
        }
    }
    if let Some(payload) = fx_drop.response.dnd_release_payload::<BrowserDragPayload>() {
        drop_browser_onto_fx(ui, s, ins, &payload);
    }
    if let Some(act) = ctx_act {
        apply_param_ctx(ui, s, act);
    }
}

pub fn mixer(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.horizontal(|u| {
        u.heading("Mixer");
        if u.button("+ Insert").clicked() {
            let idx = s.song.add_mixer_insert();
            ui.sel_insert = idx;
            s.set_inspect_insert(idx);
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
    if s.rt.inspect_insert != ui.sel_insert {
        s.set_inspect_insert(ui.sel_insert);
    }
    let pdc = s.song.compute_pdc();
    let linked_ins = s
        .song
        .channels
        .get(ui.sel_ch)
        .map(|c| c.mixer_insert)
        .unwrap_or(0);

    let mut clear_clips = false;
    let mut mix_ctx_act: Option<ParamCtxAction> = None;
    egui::ScrollArea::horizontal()
        .id_salt("mixer_strips_scroll")
        .show(u, |u| {
            u.horizontal(|u| {
                for i in 0..s.song.mixer_tracks.len() {
                    let is_sel = ui.sel_insert == i;
                    let is_linked = linked_ins == i;
                    u.group(|u| {
                        u.set_width(82.0);
                        u.vertical_centered(|u| {
                            let label = if is_linked {
                                format!("* {}", s.song.mixer_tracks[i].name)
                            } else {
                                s.song.mixer_tracks[i].name.clone()
                            };
                            let peak =
                                s.rt.mix_peaks
                                    .get(i)
                                    .copied()
                                    .unwrap_or(0.0)
                                    .clamp(0.0, 1.0);
                            let rms = s.rt.mix_rms.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
                            let clipped = s.rt.mix_clips.get(i).copied().unwrap_or(false);
                            u.horizontal(|u| {
                                if u.selectable_label(is_sel, label).clicked() {
                                    ui.sel_insert = i;
                                    s.set_inspect_insert(i);
                                }
                                let clip_col = if clipped {
                                    crate::ui_widgets::ACCENT_RED
                                } else {
                                    crate::ui_widgets::TEXT_MUTED
                                };
                                if u.add(
                                    egui::Button::new(
                                        egui::RichText::new("CL").size(9.0).color(clip_col),
                                    )
                                    .frame(false),
                                )
                                .on_hover_text("Click to clear clip indicators")
                                .clicked()
                                {
                                    clear_clips = true;
                                }
                            });
                            u.horizontal(|u| {
                                if crate::ui_widgets::led_toggle(
                                    u,
                                    s.song.mixer_tracks[i].mute,
                                    "M",
                                    crate::ui_widgets::ACCENT_RED,
                                    egui::vec2(21.0, 18.0),
                                )
                                .on_hover_text("Mute insert")
                                .clicked()
                                {
                                    s.song.mixer_tracks[i].mute = !s.song.mixer_tracks[i].mute;
                                }
                                if crate::ui_widgets::led_toggle(
                                    u,
                                    s.song.mixer_tracks[i].solo,
                                    "S",
                                    crate::ui_widgets::ACCENT_AMBER,
                                    egui::vec2(21.0, 18.0),
                                )
                                .on_hover_text("Solo insert")
                                .clicked()
                                {
                                    s.song.mixer_tracks[i].solo = !s.song.mixer_tracks[i].solo;
                                }
                                if crate::ui_widgets::led_toggle(
                                    u,
                                    s.song.mixer_tracks[i].polarity_inv,
                                    "Ø",
                                    crate::ui_widgets::ACCENT_BLUE,
                                    egui::vec2(21.0, 18.0),
                                )
                                .on_hover_text("Polarity invert")
                                .clicked()
                                {
                                    s.song.mixer_tracks[i].polarity_inv =
                                        !s.song.mixer_tracks[i].polarity_inv;
                                }
                            });
                            let vol_r = crate::ui_widgets::daw_fader_meter(
                                u,
                                &mut s.song.mixer_tracks[i].volume,
                                crate::ui_widgets::FaderSpec {
                                    range: 0.0..=1.5,
                                    default: 1.0,
                                    peak,
                                    rms,
                                    accent: crate::ui_widgets::ACCENT_GREEN,
                                    height: 110.0,
                                },
                            )
                            .on_hover_text("Insert gain (double-click to reset)");
                            collect_param_ctx(&vol_r, AutoTarget::InsertVol(i), &mut mix_ctx_act);
                            u.label(
                                egui::RichText::new(fx::format_db(s.song.mixer_tracks[i].volume))
                                    .monospace()
                                    .size(10.0)
                                    .color(crate::ui_widgets::TEXT_MUTED),
                            );
                            u.horizontal(|u| {
                                let pan_r = crate::ui_widgets::daw_knob(
                                    u,
                                    &mut s.song.mixer_tracks[i].pan,
                                    -1.0..=1.0,
                                    0.0,
                                    "Pan",
                                    crate::ui_widgets::ACCENT_BLUE,
                                )
                                .on_hover_text(format!(
                                    "Pan: {:+.0}%",
                                    s.song.mixer_tracks[i].pan * 100.0
                                ));
                                collect_param_ctx(
                                    &pan_r,
                                    AutoTarget::InsertPan(i),
                                    &mut mix_ctx_act,
                                );
                                crate::ui_widgets::daw_knob(
                                    u,
                                    &mut s.song.mixer_tracks[i].stereo_sep,
                                    -1.0..=1.0,
                                    0.0,
                                    "Sep",
                                    crate::ui_widgets::ACCENT_AMBER,
                                )
                                .on_hover_text(format!(
                                    "Stereo separation: {:+.0}%",
                                    s.song.mixer_tracks[i].stereo_sep * 100.0
                                ));
                            });
                            u.horizontal(|u| {
                                crate::ui_widgets::daw_knob(
                                    u,
                                    &mut s.song.mixer_tracks[i].delay_send,
                                    0.0..=1.0,
                                    0.0,
                                    "Dly",
                                    crate::ui_widgets::ACCENT_GREEN,
                                )
                                .on_hover_text(format!(
                                    "Delay send: {:.0}%",
                                    s.song.mixer_tracks[i].delay_send * 100.0
                                ));
                                crate::ui_widgets::daw_knob(
                                    u,
                                    &mut s.song.mixer_tracks[i].reverb_send,
                                    0.0..=1.0,
                                    0.0,
                                    "Vrb",
                                    crate::ui_widgets::ACCENT_GREEN,
                                )
                                .on_hover_text(format!(
                                    "Reverb send: {:.0}%",
                                    s.song.mixer_tracks[i].reverb_send * 100.0
                                ));
                            });
                            let total_lat = pdc.total_out.get(i).copied().unwrap_or(0);
                            u.small(format!(
                                "pdc:{total_lat} ({} fx)",
                                s.song.mixer_tracks[i].fx.len()
                            ));
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
    if clear_clips {
        s.clear_mix_clips();
    }
    if let Some(act) = mix_ctx_act {
        apply_param_ctx(ui, s, act);
    }

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
            let eff_lat = s.song.mixer_tracks[ins].effective_latency_samples();
            let comp = pdc.direct_delay.get(ins).copied().unwrap_or(0);
            let out_lat = pdc.total_out.get(ins).copied().unwrap_or(0);
            u.label(format!(
                "Eff: {eff_lat} smp | PDC: +{comp}, out {out_lat} smp"
            ));
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

    // 10-slot Insert FX Rack for the selected insert.
    fx_rack_panel(ui, s, ins, &names, u);

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

fn is_audio_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "wav" | "flac" | "mp3" | "ogg" | "aiff" | "aif"
        )
    })
}

pub fn build_browser_tree(dir: &Path, depth: u8) -> Vec<BrowserNode> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs_out = Vec::new();
    let mut files_out = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() && depth < 3 {
            let children = build_browser_tree(&p, depth + 1);
            if !children.is_empty() {
                dirs_out.push(BrowserNode::Dir {
                    name: name.to_string(),
                    children,
                });
            }
        } else if p.is_file() && is_audio_file(&p) {
            files_out.push(BrowserNode::File {
                name: name.to_string(),
                path: p.to_string_lossy().into_owned(),
            });
        }
    }
    dirs_out.sort_by(|a, b| match (a, b) {
        (BrowserNode::Dir { name: a, .. }, BrowserNode::Dir { name: b, .. }) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    });
    files_out.sort_by(|a, b| match (a, b) {
        (BrowserNode::File { name: a, .. }, BrowserNode::File { name: b, .. }) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    });
    dirs_out.extend(files_out);
    dirs_out
}

pub fn matches_browser_filter(
    name: &str,
    path: &str,
    search: &str,
    cat: BrowserCategory,
    fav_only: bool,
    prefs: &BrowserPrefs,
) -> bool {
    if fav_only && !prefs.favorites.iter().any(|f| f == path) {
        return false;
    }
    let q = search.trim().to_ascii_lowercase();
    let low_name = name.to_ascii_lowercase();
    let low_path = path.to_ascii_lowercase();
    if !q.is_empty() && !low_name.contains(&q) && !low_path.contains(&q) {
        return false;
    }
    let contains_any = |keys: &[&str]| {
        keys.iter()
            .any(|k| low_name.contains(k) || low_path.contains(k))
    };
    match cat {
        BrowserCategory::All => true,
        BrowserCategory::Drums => contains_any(&[
            "kick", "snare", "clap", "hat", "drum", "perc", "808", "rim", "crash", "tom",
        ]),
        BrowserCategory::Bass => contains_any(&["bass", "sub", "808", "low"]),
        BrowserCategory::Fx => contains_any(&[
            "fx", "riser", "sweep", "impact", "verb", "delay", "eq", "comp", "sat", "mod",
        ]),
        BrowserCategory::Presets => low_path.starts_with("preset:") || low_path.ends_with(".json"),
        BrowserCategory::Plugins => low_path.starts_with("clap:") || low_path.ends_with(".clap"),
    }
}

pub fn drop_browser_onto_channel(
    ui: &mut AppUi,
    s: &mut AppState,
    target_ch: Option<usize>,
    payload: &BrowserDragPayload,
) -> Option<usize> {
    match payload {
        BrowserDragPayload::SamplePath(path) => match sample_into_song(s, path) {
            Ok(s_idx) => {
                let s_name = s.song.samples[s_idx].name.clone();
                if let Some(ch) = target_ch
                    && let Some(c) = s.song.channels.get_mut(ch)
                {
                    c.kind = ChannelKind::Sampler {
                        sample: Some(s_idx),
                    };
                    if c.name.starts_with("Sampler") {
                        c.name = s_name.clone();
                    }
                    ui.sel_ch = ch;
                    ui.status = format!("loaded {s_name} into {}", c.name);
                    return Some(ch);
                }
                let idx = s.song.channels.len();
                let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
                let c = Channel {
                    name: s_name.clone(),
                    kind: ChannelKind::Sampler {
                        sample: Some(s_idx),
                    },
                    color: palette_color(idx),
                    cutoff: 12000.0,
                    mixer_insert: ins,
                    ..Default::default()
                };
                let ch = s.song.add_channel(c);
                ui.sel_ch = ch;
                ui.status = format!("created Sampler {s_name}");
                Some(ch)
            }
            Err(e) => {
                ui.status = format!("drop load failed: {e}");
                None
            }
        },
        BrowserDragPayload::PresetIdx(p_idx) => {
            let presets = factory_presets();
            let preset = presets.get(*p_idx)?;
            if let Some(ch) = target_ch
                && let Some(c) = s.song.channels.get_mut(ch)
            {
                apply_preset_to_channel(c, preset);
                let cname = c.name.clone();
                s.rebuild_channel(ch);
                ui.sel_ch = ch;
                ui.status = format!("loaded preset {} onto {cname}", preset.name);
                return Some(ch);
            }
            let idx = s.song.channels.len();
            let ins = (idx + 1).min(s.song.mixer_tracks.len().saturating_sub(1));
            let mut c = Channel {
                color: palette_color(idx),
                mixer_insert: ins,
                ..Default::default()
            };
            apply_preset_to_channel(&mut c, preset);
            let ch = s.song.add_channel(c);
            s.rebuild_channel(ch);
            ui.sel_ch = ch;
            ui.status = format!("added channel {}", preset.name);
            Some(ch)
        }
        BrowserDragPayload::ClapPlugin(idx) => {
            let desc = ui.clap_catalog.plugins.get(*idx)?.clone();
            if let Some(ch) = target_ch
                && let Some(c) = s.song.channels.get_mut(ch)
            {
                c.kind = ChannelKind::ClapPlugin(clap_cfg_from_desc(&desc));
                c.name = desc.name.clone();
                sync_clap_plugins(ui, s);
                ui.sel_ch = ch;
                ui.show_ch_settings = true;
                ui.status = format!("loaded CLAP {} onto channel {}", desc.name, ch + 1);
                return Some(ch);
            }
            Some(add_clap_channel_from_desc(ui, s, &desc))
        }
        BrowserDragPayload::FxChoice(_) => {
            ui.status = "drop FX modules onto Mixer FX Rack".into();
            None
        }
    }
}

pub fn drop_browser_onto_playlist(
    ui: &mut AppUi,
    s: &mut AppState,
    track: usize,
    tick: u32,
    payload: &BrowserDragPayload,
) -> Option<u64> {
    let BrowserDragPayload::SamplePath(path) = payload else {
        ui.status = "drop audio samples onto Playlist".into();
        return None;
    };
    match sample_into_song(s, path) {
        Ok(s_idx) => {
            let smp_len = s.song.samples[s_idx].data.len();
            let smp_sr = s.song.samples[s_idx].sr.max(1.0);
            let cid = s.song.add_audio_clip(track, tick, s_idx, 1.0);
            let spt = (smp_sr * 60.0 / s.song.bpm.max(30.0) / 16.0).max(1.0);
            let ticks = ((smp_len as f32) / spt).ceil().max(4.0) as u32;
            if let Some(c) = s.song.clips.iter_mut().find(|c| c.id == cid) {
                c.len_ticks = ticks;
            }
            ui.sel_clip = Some(cid);
            ui.sel_track = track;
            ui.status = format!("dropped {} onto Playlist", s.song.samples[s_idx].name);
            Some(cid)
        }
        Err(e) => {
            ui.status = format!("drop failed: {e}");
            None
        }
    }
}

pub fn drop_browser_onto_fx(
    ui: &mut AppUi,
    s: &mut AppState,
    ins: usize,
    payload: &BrowserDragPayload,
) -> bool {
    match payload {
        BrowserDragPayload::FxChoice(choice) => {
            let Some(track) = s.song.mixer_tracks.get_mut(ins) else {
                return false;
            };
            let kind = fx_kind_from_choice(*choice);
            let nm = kind.default_name().to_string();
            if track.add_fx(EffectSlot::new(kind)) {
                ui.sel_fx_slot = track.fx.len().saturating_sub(1);
                ui.status = format!("added {nm} to {}", track.name);
                true
            } else {
                ui.status = "FX rack full".into();
                false
            }
        }
        BrowserDragPayload::ClapPlugin(idx) => {
            let Some(desc) = ui.clap_catalog.plugins.get(*idx).cloned() else {
                return false;
            };
            add_clap_fx_from_desc(ui, s, ins, &desc)
        }
        _ => {
            ui.status = "drop FX modules or CLAP FX onto Mixer FX Rack".into();
            false
        }
    }
}

fn preview_browser_sample(ui: &mut AppUi, s: &mut AppState, path: &str) {
    match sample_into_song(s, path) {
        Ok(idx) => {
            if ui.browser_sync_preview {
                s.live_sample_synced(idx);
                ui.status = format!("synced preview {path}");
            } else {
                s.live_sample(idx);
                ui.status = format!("preview {path}");
            }
        }
        Err(e) => ui.status = format!("preview failed: {e}"),
    }
}

fn toggle_browser_fav(ui: &mut AppUi, path: &str) {
    if let Some(pos) = ui.browser_prefs.favorites.iter().position(|f| f == path) {
        ui.browser_prefs.favorites.remove(pos);
    } else {
        ui.browser_prefs.favorites.push(path.to_string());
    }
    if let Some(cfg) = config_dir() {
        ui.browser_prefs.save_to(&cfg);
    }
}

fn render_browser_nodes(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui, nodes: &[BrowserNode]) {
    for node in nodes {
        match node {
            BrowserNode::Dir { name, children } => {
                egui::CollapsingHeader::new(format!("📁 {name}"))
                    .id_salt(("bdir", name))
                    .show(u, |u| render_browser_nodes(ui, s, u, children));
            }
            BrowserNode::File { name, path } => {
                if !matches_browser_filter(
                    name,
                    path,
                    &ui.browser_search,
                    ui.browser_category,
                    ui.browser_fav_only,
                    &ui.browser_prefs,
                ) {
                    continue;
                }
                u.horizontal(|u| {
                    let fav = ui.browser_prefs.favorites.iter().any(|f| f == path);
                    if u.small_button(if fav { "★" } else { "☆" })
                        .on_hover_text("Toggle favorite")
                        .clicked()
                    {
                        toggle_browser_fav(ui, path);
                    }
                    let item = u.add(egui::Label::new(name).sense(egui::Sense::click_and_drag()));
                    if item.dragged() {
                        item.dnd_set_drag_payload(BrowserDragPayload::SamplePath(path.clone()));
                    }
                    if item.double_clicked() {
                        preview_browser_sample(ui, s, path);
                    }
                    item.on_hover_text("Drag to Channel Rack or Playlist; double-click to preview");
                    if u.small_button("▶").on_hover_text("Preview").clicked() {
                        preview_browser_sample(ui, s, path);
                    }
                    if u.small_button("→ Ch")
                        .on_hover_text("Load into selected channel")
                        .clicked()
                    {
                        let ch = ui.sel_ch;
                        drop_browser_onto_channel(
                            ui,
                            s,
                            Some(ch),
                            &BrowserDragPayload::SamplePath(path.clone()),
                        );
                    }
                });
            }
        }
    }
}

pub fn browser(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.heading("Browser");
    u.horizontal_wrapped(|u| {
        if u.button("Folder...").clicked()
            && let Some(p) = rfd::FileDialog::new().pick_folder()
        {
            ui.browser_dir = p.to_string_lossy().into_owned();
            ui.browser_tree_dirty = true;
        }
        if u.button("↻").on_hover_text("Rescan folder").clicked() {
            ui.browser_tree_dirty = true;
        }
        if u.button("+ Pin")
            .on_hover_text("Pin active folder")
            .clicked()
        {
            let dir = ui.browser_dir.clone();
            if !dir.is_empty() && !ui.browser_prefs.pinned_dirs.contains(&dir) {
                ui.browser_prefs.pinned_dirs.push(dir);
                if let Some(cfg) = config_dir() {
                    ui.browser_prefs.save_to(&cfg);
                }
            }
        }
        if u.button("~/Music").clicked() {
            ui.browser_dir = home_music_dir();
            ui.browser_tree_dirty = true;
        }
        if u.button("Scan CLAP")
            .on_hover_text("Scan standard CLAP plugin folders")
            .clicked()
        {
            scan_all_clap_plugins(ui);
        }
        if u.button(".clap...")
            .on_hover_text("Scan and register a .clap plugin file")
            .clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("CLAP Plugin", &["clap"])
                .pick_file()
        {
            match register_clap_file_into_catalog(ui, &p) {
                Ok(descs) => {
                    ui.browser_category = BrowserCategory::Plugins;
                    ui.status = format!("registered {} CLAP plugin(s)", descs.len());
                }
                Err(e) => ui.status = format!("CLAP scan failed: {e}"),
            }
        }
    });

    if !ui.browser_prefs.pinned_dirs.is_empty() {
        let pins = ui.browser_prefs.pinned_dirs.clone();
        let mut rm_pin: Option<String> = None;
        u.horizontal_wrapped(|u| {
            u.label("Pins:");
            for pdir in pins {
                let lbl = Path::new(&pdir)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&pdir);
                if u.selectable_label(ui.browser_dir == pdir, lbl)
                    .on_hover_text(&pdir)
                    .clicked()
                {
                    ui.browser_dir = pdir.clone();
                    ui.browser_tree_dirty = true;
                }
                if u.small_button("×").on_hover_text("Unpin").clicked() {
                    rm_pin = Some(pdir);
                }
            }
        });
        if let Some(p) = rm_pin {
            ui.browser_prefs.pinned_dirs.retain(|d| d != &p);
            if let Some(cfg) = config_dir() {
                ui.browser_prefs.save_to(&cfg);
            }
        }
    }

    u.horizontal(|u| {
        u.label("Search:");
        u.add(egui::TextEdit::singleline(&mut ui.browser_search).desired_width(135.0));
        u.toggle_value(&mut ui.browser_fav_only, "★")
            .on_hover_text("Favorites only");
        u.toggle_value(&mut ui.browser_sync_preview, "Sync")
            .on_hover_text("Tempo-synced loop preview");
    });

    u.horizontal_wrapped(|u| {
        for cat in BrowserCategory::ALL {
            u.selectable_value(&mut ui.browser_category, cat, cat.name());
        }
    });

    if ui.browser_tree_dirty {
        ui.browser_tree = build_browser_tree(Path::new(&ui.browser_dir), 0);
        ui.browser_tree_dirty = false;
    }

    let tree = ui.browser_tree.clone();
    if !tree.is_empty() {
        render_browser_nodes(ui, s, u, &tree);
    } else if !matches!(
        ui.browser_category,
        BrowserCategory::Presets | BrowserCategory::Fx | BrowserCategory::Plugins
    ) {
        u.small(format!("No audio files in {}", ui.browser_dir));
    }

    if matches!(
        ui.browser_category,
        BrowserCategory::All | BrowserCategory::Presets
    ) {
        let presets = factory_presets();
        egui::CollapsingHeader::new("Factory Presets")
            .default_open(ui.browser_category == BrowserCategory::Presets)
            .show(u, |u| {
                for (pi, preset) in presets.iter().enumerate() {
                    let key = format!("preset:{}", preset.name);
                    if !matches_browser_filter(
                        &preset.name,
                        &key,
                        &ui.browser_search,
                        ui.browser_category,
                        ui.browser_fav_only,
                        &ui.browser_prefs,
                    ) {
                        continue;
                    }
                    u.horizontal(|u| {
                        let fav = ui.browser_prefs.favorites.iter().any(|f| f == &key);
                        if u.small_button(if fav { "★" } else { "☆" }).clicked() {
                            toggle_browser_fav(ui, &key);
                        }
                        let resp = u.add(
                            egui::Label::new(&preset.name).sense(egui::Sense::click_and_drag()),
                        );
                        if resp.dragged() {
                            resp.dnd_set_drag_payload(BrowserDragPayload::PresetIdx(pi));
                        }
                        if resp.double_clicked() {
                            let ch = ui.sel_ch;
                            drop_browser_onto_channel(
                                ui,
                                s,
                                Some(ch),
                                &BrowserDragPayload::PresetIdx(pi),
                            );
                        }
                    });
                }
            });
    }

    if matches!(
        ui.browser_category,
        BrowserCategory::All | BrowserCategory::Fx
    ) {
        egui::CollapsingHeader::new("FX Modules")
            .default_open(ui.browser_category == BrowserCategory::Fx)
            .show(u, |u| {
                for (fi, &name) in FX_CHOICE_NAMES.iter().enumerate() {
                    let key = format!("fx:{name}");
                    if !matches_browser_filter(
                        name,
                        &key,
                        &ui.browser_search,
                        ui.browser_category,
                        ui.browser_fav_only,
                        &ui.browser_prefs,
                    ) {
                        continue;
                    }
                    u.horizontal(|u| {
                        let resp =
                            u.add(egui::Label::new(name).sense(egui::Sense::click_and_drag()));
                        if resp.dragged() {
                            resp.dnd_set_drag_payload(BrowserDragPayload::FxChoice(fi));
                        }
                        if resp.double_clicked() {
                            let ins = ui.sel_insert;
                            drop_browser_onto_fx(ui, s, ins, &BrowserDragPayload::FxChoice(fi));
                        }
                    });
                }
            });
    }

    if matches!(
        ui.browser_category,
        BrowserCategory::All | BrowserCategory::Plugins
    ) {
        let plugins = ui.clap_catalog.plugins.clone();
        let last_err = ui.clap_catalog.last_errors.last().cloned();
        let err_count = ui.clap_catalog.last_errors.len();
        egui::CollapsingHeader::new("CLAP Plugins")
            .default_open(ui.browser_category == BrowserCategory::Plugins)
            .show(u, |u| {
                if plugins.is_empty() {
                    u.small("No CLAP plugins scanned yet. Click Scan CLAP or .clap...");
                }
                if err_count > 0 {
                    let tail = last_err.unwrap_or_default();
                    u.small(format!("{err_count} scan errors (last: {tail})"));
                }
                for (ci, desc) in plugins.iter().enumerate() {
                    let key = format!("clap:{}:{}", desc.path, desc.id);
                    if !matches_browser_filter(
                        &desc.name,
                        &key,
                        &ui.browser_search,
                        ui.browser_category,
                        ui.browser_fav_only,
                        &ui.browser_prefs,
                    ) {
                        continue;
                    }
                    let kind_tag = match (desc.is_instrument, desc.is_effect) {
                        (true, true) => "Inst+FX",
                        (true, false) => "Inst",
                        (false, true) => "FX",
                        (false, false) => "Plugin",
                    };
                    let tip = format!(
                        "{} v{} by {}\nID: {}\nFeatures: {}\n{} | note-in:{} audio:{}/{}\n{}",
                        desc.name,
                        if desc.version.is_empty() {
                            "?"
                        } else {
                            &desc.version
                        },
                        if desc.vendor.is_empty() {
                            "Unknown"
                        } else {
                            &desc.vendor
                        },
                        desc.id,
                        if desc.features.is_empty() {
                            "-".to_string()
                        } else {
                            desc.features.join(", ")
                        },
                        kind_tag,
                        if desc.has_note_in { "yes" } else { "no" },
                        if desc.has_audio_in { "in" } else { "-" },
                        if desc.has_audio_out { "out" } else { "-" },
                        desc.path,
                    );
                    u.horizontal(|u| {
                        let fav = ui.browser_prefs.favorites.iter().any(|f| f == &key);
                        if u.small_button(if fav { "★" } else { "☆" }).clicked() {
                            toggle_browser_fav(ui, &key);
                        }
                        let resp = u
                            .add(egui::Label::new(&desc.name).sense(egui::Sense::click_and_drag()))
                            .on_hover_text(&tip);
                        if resp.dragged() {
                            resp.dnd_set_drag_payload(BrowserDragPayload::ClapPlugin(ci));
                        }
                        if resp.double_clicked() {
                            if desc.is_effect && !desc.is_instrument {
                                let ins = ui.sel_insert;
                                add_clap_fx_from_desc(ui, s, ins, desc);
                            } else {
                                add_clap_channel_from_desc(ui, s, desc);
                            }
                        }
                        if u.small_button("→ Ch")
                            .on_hover_text("Add as CLAP instrument channel")
                            .clicked()
                        {
                            add_clap_channel_from_desc(ui, s, desc);
                        }
                        if u.small_button("→ FX")
                            .on_hover_text("Add to selected mixer insert FX rack")
                            .clicked()
                        {
                            let ins = ui.sel_insert;
                            add_clap_fx_from_desc(ui, s, ins, desc);
                        }
                    });
                }
            });
    }

    u.separator();
    u.horizontal_wrapped(|u| {
        if u.button("Open Sample...").clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("Audio", &["wav", "flac", "mp3", "ogg", "aiff"])
                .pick_file()
        {
            let ch = ui.sel_ch;
            drop_browser_onto_channel(
                ui,
                s,
                Some(ch),
                &BrowserDragPayload::SamplePath(p.to_string_lossy().into_owned()),
            );
        }
        if !s.song.samples.is_empty() && u.button("Edison Editor").clicked() {
            let idx = ui.clip_sample.min(s.song.samples.len() - 1);
            ui.edison_sample = Some(idx);
            ui.edison_sel = (0.0, 1.0);
            ui.edison_slices.clear();
        }
        u.small(format!("{} samples loaded", s.song.samples.len()));
    });
}

pub fn edison_editor_ui(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    let Some(mut s_idx) = ui.edison_sample else {
        return;
    };
    if s.song.samples.is_empty() {
        u.label("No samples loaded.");
        if u.button("Close").clicked() {
            ui.edison_sample = None;
        }
        return;
    }
    s_idx = s_idx.min(s.song.samples.len() - 1);
    ui.edison_sample = Some(s_idx);

    let names: Vec<String> = s.song.samples.iter().map(|sm| sm.name.clone()).collect();
    u.horizontal_wrapped(|u| {
        egui::ComboBox::from_id_salt("edison_sample_pick")
            .selected_text(names.get(s_idx).cloned().unwrap_or_default())
            .show_ui(u, |u| {
                for (i, nm) in names.iter().enumerate() {
                    if u.selectable_label(s_idx == i, nm).clicked() {
                        ui.edison_sample = Some(i);
                        ui.edison_sel = (0.0, 1.0);
                        ui.edison_slices.clear();
                    }
                }
            });
        u.add(egui::Slider::new(&mut ui.edison_zoom, 1.0..=16.0).text("Zoom"));
        if u.button("Preview").clicked() {
            s.live_sample(s_idx);
        }
        if u.button("Close").clicked() {
            ui.edison_sample = None;
        }
    });
    let Some(s_idx) = ui.edison_sample else {
        return;
    };

    // Draw zoomed waveform and allow click/drag selection.
    let smp_len = s.song.samples[s_idx].data.len();
    let smp_sr = s.song.samples[s_idx].sr.max(1.0);
    let view_frac = (1.0 / ui.edison_zoom.clamp(1.0, 16.0)).clamp(0.0625, 1.0);
    let sel_center = ((ui.edison_sel.0 + ui.edison_sel.1) * 0.5).clamp(0.0, 1.0);
    let v0 = (sel_center - view_frac * 0.5).clamp(0.0, 1.0 - view_frac);
    let v1 = (v0 + view_frac).min(1.0);

    let avail_w = u.available_width().clamp(260.0, 760.0);
    let (rect, resp) =
        u.allocate_exact_size(egui::vec2(avail_w, 96.0), egui::Sense::click_and_drag());
    let p = u.painter_at(rect);
    p.rect_filled(rect, 3.0, egui::Color32::from_gray(14));
    let mid_y = rect.center().y;
    p.line_segment(
        [egui::pos2(rect.min.x, mid_y), egui::pos2(rect.max.x, mid_y)],
        egui::Stroke::new(1.0, egui::Color32::from_gray(42)),
    );

    let to_x = |frac: f32| rect.min.x + ((frac - v0) / (v1 - v0).max(1e-4)) * rect.width();
    let from_x = |x: f32| {
        let u_pos = ((x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0);
        (v0 + u_pos * (v1 - v0)).clamp(0.0, 1.0)
    };

    if let Some(smp) = s.song.samples.get(s_idx) {
        let n_cols = (rect.width() as usize).clamp(64, 512);
        let len = smp.data.len();
        if len > 0 {
            for col in 0..n_cols {
                let f0 = v0 + (col as f32 / n_cols as f32) * (v1 - v0);
                let f1 = v0 + ((col + 1) as f32 / n_cols as f32) * (v1 - v0);
                let i0 = ((f0 * len as f32) as usize).min(len - 1);
                let i1 = ((f1 * len as f32).ceil() as usize).clamp(i0 + 1, len);
                let mut pk = 0.0f32;
                let stride = ((i1 - i0) / 16).max(1);
                for idx in (i0..i1).step_by(stride) {
                    let vl = smp.data[idx].abs();
                    let vr = smp.data_r.get(idx).copied().unwrap_or(0.0).abs();
                    pk = pk.max(vl.max(vr));
                }
                let bx = rect.min.x + (col as f32 / n_cols as f32) * rect.width();
                let hh = (pk.clamp(0.0, 1.0) * rect.height() * 0.46).max(1.0);
                p.line_segment(
                    [egui::pos2(bx, mid_y - hh), egui::pos2(bx, mid_y + hh)],
                    egui::Stroke::new(1.2, egui::Color32::from_rgb(105, 195, 240)),
                );
            }
        }
    }

    // Selection overlay.
    let sx0 = to_x(ui.edison_sel.0).clamp(rect.min.x, rect.max.x);
    let sx1 = to_x(ui.edison_sel.1).clamp(rect.min.x, rect.max.x);
    if sx1 > sx0 + 1.0 {
        p.rect_filled(
            egui::Rect::from_min_max(egui::pos2(sx0, rect.min.y), egui::pos2(sx1, rect.max.y)),
            0.0,
            egui::Color32::from_rgba_unmultiplied(80, 175, 240, 48),
        );
    }
    for &mk in &ui.edison_slices {
        if smp_len > 0 {
            let mf = mk as f32 / smp_len as f32;
            if mf >= v0 && mf <= v1 {
                let mx = to_x(mf);
                p.line_segment(
                    [egui::pos2(mx, rect.min.y), egui::pos2(mx, rect.max.y)],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(245, 185, 65)),
                );
            }
        }
    }

    if resp.drag_started()
        && let Some(p0) = u.ctx().input(|i| i.pointer.press_origin())
    {
        let f0 = from_x(p0.x);
        ui.edison_sel = (f0, (f0 + 0.01).min(1.0));
    } else if resp.dragged()
        && let Some(p0) = u.ctx().input(|i| i.pointer.press_origin())
        && let Some(pos) = resp.hover_pos()
    {
        let a = from_x(p0.x);
        let b = from_x(pos.x);
        ui.edison_sel = if a <= b {
            (a, b.max(a + 0.005).min(1.0))
        } else {
            (b, a.max(b + 0.005).min(1.0))
        };
    }

    u.horizontal_wrapped(|u| {
        let mut s0 = ui.edison_sel.0;
        let mut s1 = ui.edison_sel.1;
        u.add(egui::Slider::new(&mut s0, 0.0..=0.98).text("Start"));
        u.add(egui::Slider::new(&mut s1, 0.02..=1.0).text("End"));
        if s1 <= s0 + 0.01 {
            s1 = (s0 + 0.01).min(1.0);
        }
        ui.edison_sel = (s0, s1);
    });

    let mut op: Option<&'static str> = None;
    u.horizontal_wrapped(|u| {
        if u.button("Detect Transients").clicked() {
            op = Some("transients");
        }
        if u.button("Trim").clicked() {
            op = Some("trim");
        }
        if u.button("Fade In").clicked() {
            op = Some("fade_in");
        }
        if u.button("Fade Out").clicked() {
            op = Some("fade_out");
        }
        if u.button("Normalize").clicked() {
            op = Some("norm");
        }
        if u.button("Remove DC").clicked() {
            op = Some("dc");
        }
        if u.button("Reverse").clicked() {
            op = Some("rev");
        }
        if u.button("Extract Slice").clicked() {
            op = Some("slice");
        }
        if u.button("→ Playlist").clicked() {
            op = Some("playlist");
        }
        if u.button("→ Selected Ch").clicked() {
            op = Some("channel");
        }
    });

    if let Some(cmd) = op {
        let (f0, f1) = ui.edison_sel;
        match cmd {
            "transients" => {
                let min_gap = (smp_sr as usize / 12).max(512);
                ui.edison_slices = s.song.samples[s_idx].detect_transients(0.15, min_gap);
                ui.status = format!("detected {} transients", ui.edison_slices.len());
            }
            "trim" => {
                s.song.samples[s_idx].trim_range_in_place(f0, f1);
                ui.edison_sel = (0.0, 1.0);
                ui.edison_slices.clear();
                ui.status = "trimmed sample".into();
            }
            "fade_in" => {
                s.song.samples[s_idx].fade_in_range_in_place(f0, f1);
                ui.status = "applied fade in".into();
            }
            "fade_out" => {
                s.song.samples[s_idx].fade_out_range_in_place(f0, f1);
                ui.status = "applied fade out".into();
            }
            "norm" => {
                s.song.samples[s_idx].normalize_in_place();
                ui.status = "normalized sample".into();
            }
            "dc" => {
                s.song.samples[s_idx].remove_dc_in_place();
                ui.status = "removed DC offset".into();
            }
            "rev" => {
                s.song.samples[s_idx].reverse_in_place();
                ui.status = "reversed sample".into();
            }
            "slice" => {
                let len = s.song.samples[s_idx].data.len();
                let i0 = (f0 * len as f32) as usize;
                let i1 = (f1 * len as f32) as usize;
                let slice_name = format!(
                    "{}_slice{}",
                    s.song.samples[s_idx].name,
                    s.song.samples.len()
                );
                let sliced = s.song.samples[s_idx].extract_slice(slice_name, i0, i1);
                s.song.samples.push(sliced);
                let new_idx = s.song.samples.len() - 1;
                ui.edison_sample = Some(new_idx);
                ui.edison_sel = (0.0, 1.0);
                ui.status = format!("extracted slice #{}", new_idx + 1);
            }
            "playlist" => {
                let track = ui
                    .sel_track
                    .min(s.song.playlist_tracks.len().saturating_sub(1));
                let cid = s.song.add_audio_clip(track, 0, s_idx, 1.0);
                let spt = (smp_sr * 60.0 / s.song.bpm.max(30.0) / 16.0).max(1.0);
                let ticks = ((smp_len as f32) / spt).ceil().max(4.0) as u32;
                if let Some(c) = s.song.clips.iter_mut().find(|c| c.id == cid) {
                    c.len_ticks = ticks;
                }
                ui.sel_clip = Some(cid);
                ui.show_playlist = true;
                sync_dock_tabs(ui);
                ui.status = "placed sample on Playlist".into();
            }
            "channel" => {
                let ch = ui.sel_ch.min(s.song.channels.len().saturating_sub(1));
                if let Some(c) = s.song.channels.get_mut(ch) {
                    c.kind = ChannelKind::Sampler {
                        sample: Some(s_idx),
                    };
                    ui.status = format!("loaded {} -> {}", s.song.samples[s_idx].name, c.name);
                }
            }
            _ => {}
        }
    }
}

pub fn edison_window(ui: &mut AppUi, s: &mut AppState, ctx: &egui::Context) {
    if ui.edison_sample.is_none() {
        return;
    }
    let mut open = true;
    egui::Window::new("Edison Sample Editor")
        .open(&mut open)
        .default_width(560.0)
        .show(ctx, |u| {
            edison_editor_ui(ui, s, u);
        });
    if !open {
        ui.edison_sample = None;
    }
}

pub fn export_modal_ui(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.horizontal_wrapped(|u| {
        u.label("Format:");
        for fmt in ExportFormat::ALL {
            if u.selectable_label(ui.export_cfg.format == fmt, fmt.name())
                .clicked()
            {
                ui.export_cfg.format = fmt;
                let base = Path::new(&ui.export_target_path)
                    .file_stem()
                    .and_then(|st| st.to_str())
                    .filter(|st| !st.is_empty())
                    .unwrap_or("mixdown");
                ui.export_target_path = format!("{base}.{}", fmt.ext());
            }
        }
    });
    u.horizontal_wrapped(|u| {
        u.label("Sample rate:");
        for (sr, lbl) in [(44100u32, "44.1 kHz"), (48000, "48 kHz"), (96000, "96 kHz")] {
            if u.selectable_label(ui.export_cfg.sample_rate == sr, lbl)
                .clicked()
            {
                ui.export_cfg.sample_rate = sr;
            }
        }
    });
    u.horizontal_wrapped(|u| {
        u.label("Oversample:");
        for (os, lbl) in [(1u8, "1x"), (2, "2x"), (4, "4x")] {
            if u.selectable_label(ui.export_cfg.oversample == os, lbl)
                .clicked()
            {
                ui.export_cfg.oversample = os;
            }
        }
    });
    u.horizontal_wrapped(|u| {
        u.label("Tail:");
        for tail in TailMode::ALL {
            if u.selectable_label(ui.export_cfg.tail_mode == tail, tail.name())
                .clicked()
            {
                ui.export_cfg.tail_mode = tail;
            }
        }
    });
    u.checkbox(
        &mut ui.export_cfg.split_mixer_tracks,
        "Split Mixer Tracks (Stem Export)",
    );
    u.separator();
    u.horizontal(|u| {
        if u.button("Export via Dialog...").clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter(ui.export_cfg.format.name(), &[ui.export_cfg.format.ext()])
                .set_file_name(&ui.export_target_path)
                .save_file()
        {
            match crate::export::export_audio(&s.song, &p.to_string_lossy(), &ui.export_cfg) {
                Ok(m) => {
                    ui.status = m;
                    ui.show_export_modal = false;
                }
                Err(e) => ui.status = format!("export failed: {e}"),
            }
        }
        if u.button("Close").clicked() {
            ui.show_export_modal = false;
        }
    });
}

pub fn export_window(ui: &mut AppUi, s: &mut AppState, ctx: &egui::Context) {
    if !ui.show_export_modal {
        return;
    }
    let mut open = true;
    egui::Window::new("Export Audio / Stems")
        .open(&mut open)
        .default_width(480.0)
        .show(ctx, |u| {
            export_modal_ui(ui, s, u);
        });
    if !open {
        ui.show_export_modal = false;
    }
}

pub fn recovery_modal_ui(ui: &mut AppUi, s: &mut AppState, u: &mut egui::Ui) {
    u.label("An autosaved session from an unexpected exit was found.");
    u.horizontal(|u| {
        if u.button("Restore Autosave").clicked() {
            if let Some(dir) = data_dir() {
                match load_autosave(&dir) {
                    Ok((song, missing)) => {
                        apply_loaded_song_to_ui(ui, s, song, None);
                        ui.status = if missing == 0 {
                            "restored autosaved project".into()
                        } else {
                            format!("restored autosave ({missing} missing samples)")
                        };
                    }
                    Err(e) => ui.status = format!("autosave restore failed: {e}"),
                }
            }
            ui.show_recovery_prompt = false;
        }
        if u.button("Discard Autosave").clicked() {
            if let Some(dir) = data_dir() {
                discard_recovery(&dir);
            }
            ui.show_recovery_prompt = false;
            ui.status = "discarded autosave".into();
        }
    });
}

pub fn recovery_window(ui: &mut AppUi, s: &mut AppState, ctx: &egui::Context) {
    if !ui.show_recovery_prompt {
        return;
    }
    egui::Window::new("Crash Recovery")
        .collapsible(false)
        .resizable(false)
        .show(ctx, |u| {
            recovery_modal_ui(ui, s, u);
        });
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
            rect.min.y + (midi_hi - midi) as f32 * PIANO_CELL_H + PIANO_CELL_H * 0.5,
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
        let y = rect.min.y + (midi_hi - 60) as f32 * PIANO_CELL_H + PIANO_CELL_H * 0.5;
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
        state.song.samples.push(crate::song::SampleRef::from_mono(
            "tone.wav",
            "tone.wav",
            vec![0.5],
            44100.0,
        ));
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
    fn channel_rack_adds_supersynth_fm_and_drumrack() {
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
        h.set_size(egui::vec2(1280.0, 720.0));
        h.step();
        h.get_by_label("+ Super").click();
        h.step();
        h.get_by_label("+ FM").click();
        h.step();
        h.get_by_label("+ DrumRack").click();
        h.step();
        let chs = &h.state().app.song.channels;
        assert_eq!(chs.len(), initial + 3);
        assert!(matches!(chs[initial].kind, ChannelKind::SuperSynth(_)));
        assert!(matches!(chs[initial + 1].kind, ChannelKind::FmSynth(_)));
        assert!(matches!(
            chs[initial + 2].kind,
            ChannelKind::DrumRack { .. }
        ));
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
    fn mixer_fx_rack_adds_reorders_and_bypasses_slots() {
        struct MState {
            ui: AppUi,
            app: AppState,
        }
        let state = MState {
            ui: AppUi::default(),
            app: AppState::new(),
        };
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut MState| {
                mixer(&mut st.ui, &mut st.app, u);
            },
            state,
        );
        h.set_size(egui::vec2(1280.0, 900.0));
        h.step();
        h.state_mut().ui.add_fx_kind = 0; // Parametric EQ
        h.get_by_label("+ FX").click();
        h.step();
        h.state_mut().ui.add_fx_kind = 1; // Compressor
        h.get_by_label("+ FX").click();
        h.step();
        assert_eq!(h.state().app.song.mixer_tracks[1].fx.len(), 2);
        assert_eq!(
            h.state().app.song.mixer_tracks[1].fx[0].name,
            "Parametric EQ"
        );
        assert_eq!(h.state().app.song.mixer_tracks[1].fx[1].name, "Compressor");

        // Reorder and bypass slot via UI buttons.
        h.get_by_label("^").click();
        h.step();
        assert_eq!(h.state().app.song.mixer_tracks[1].fx[0].name, "Compressor");
        h.get_by_label("Bypass").click();
        h.step();
        assert!(h.state().app.song.mixer_tracks[1].fx[0].bypass);
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

    #[test]
    fn piano_marquee_select_legato_slice_and_mute() {
        let mut state = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        state.app.song.patterns[0].notes[4].push(Note::new(0, 4, 60, 0.8));
        state.app.song.patterns[0].notes[4].push(Note::new(16, 4, 64, 0.8));
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                let r = piano_canvas(&mut st.ui, &mut st.app, 4, 0, u);
                st.rect = Some(r);
            },
            state,
        );
        h.set_size(egui::vec2(1200.0, 800.0));
        h.step();

        // Select all, legato extends note 0 to touch tick 16.
        {
            let st = h.state_mut();
            select_all_notes(&mut st.ui, &st.app);
            assert_eq!(st.ui.sel_notes, vec![0, 1]);
            assert!(legato_selected_notes(&mut st.ui, &mut st.app));
            assert_eq!(st.app.song.patterns[0].notes[4][0].len, 16);
        }

        // Slice tool splits note 0 at tick 8.
        h.state_mut().ui.tool = PianoTool::Slice;
        let cut_pos = cell_center(&h, 8, 60);
        click_at(&mut h, cut_pos);
        assert_eq!(h.state().app.song.patterns[0].notes[4].len(), 3);
        assert_eq!(h.state().app.song.patterns[0].notes[4][0].len, 8);
        assert_eq!(h.state().app.song.patterns[0].notes[4][1].tick, 8);

        // Mute tool toggles mute on the second half.
        h.state_mut().ui.tool = PianoTool::Mute;
        let mute_pos = cell_center(&h, 9, 60);
        click_at(&mut h, mute_pos);
        assert!(h.state().app.song.patterns[0].notes[4][1].mute);
    }

    #[test]
    fn piano_slide_and_property_lane_edit() {
        let mut state = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        state.ui.next_slide = true;
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                let r = piano_canvas(&mut st.ui, &mut st.app, 4, 0, u);
                st.rect = Some(r);
                note_property_lane(&mut st.ui, &mut st.app, 4, 0, u);
            },
            state,
        );
        h.set_size(egui::vec2(1200.0, 800.0));
        h.step();
        let p = cell_center(&h, 4, 62);
        click_at(&mut h, p);
        assert!(h.state().app.song.patterns[0].notes[4][0].slide);

        // Non-destructive modal preview & cancel restores backup.
        {
            let st = h.state_mut();
            open_piano_modal(&mut st.ui, &mut st.app, 4, 0, PianoModalKind::Flip);
            assert!(st.ui.modal.is_some());
        }
    }

    #[test]
    fn phase5_edison_trim_normalize_and_send_to_playlist() {
        let mut state = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        let pcm: Vec<f32> = (0..400).map(|i| if i < 100 { 0.1 } else { 0.25 }).collect();
        state
            .app
            .song
            .samples
            .push(crate::song::SampleRef::from_mono(
                "vox.wav", "vox.wav", pcm, 44100.0,
            ));
        state.ui.edison_sample = Some(0);
        state.ui.edison_sel = (0.25, 0.75);

        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                edison_editor_ui(&mut st.ui, &mut st.app, u);
            },
            state,
        );
        h.set_size(egui::vec2(820.0, 520.0));
        h.step();

        h.get_by_label("Trim").click();
        h.step();
        assert_eq!(h.state().app.song.samples[0].data.len(), 200);

        h.get_by_label("Normalize").click();
        h.step();
        let pk = h.state().app.song.samples[0]
            .data
            .iter()
            .map(|x| x.abs())
            .fold(0.0f32, f32::max);
        assert!((pk - 0.98).abs() < 0.01);

        h.get_by_label("→ Playlist").click();
        h.step();
        assert_eq!(h.state().app.song.clips.len(), 1);
        assert!(matches!(
            h.state().app.song.clips[0].kind,
            ClipKind::Automation { .. } | ClipKind::Audio { .. }
        ));
        assert!(matches!(
            h.state().app.song.clips[0].kind,
            ClipKind::Audio { sample: 0, .. }
        ));
    }

    #[test]
    fn phase5_midi_learn_and_automation_clip() {
        let mut ui = AppUi::default();
        let mut s = AppState::new();
        apply_param_ctx(
            &mut ui,
            &mut s,
            ParamCtxAction::CreateAuto(AutoTarget::ChannelVol(0)),
        );
        assert_eq!(s.song.clips.len(), 1);
        assert!(matches!(
            s.song.clips[0].kind,
            ClipKind::Automation {
                target: AutoTarget::ChannelVol(0),
                ..
            }
        ));

        apply_param_ctx(
            &mut ui,
            &mut s,
            ParamCtxAction::MidiLearn(AutoTarget::ChannelCut(0)),
        );
        assert_eq!(ui.midi_learn.armed_target, Some(AutoTarget::ChannelCut(0)));
        assert!(ui.show_io_panel);
        ui.midi_learn.pending_curve = MapCurve::Log;
        assert!(ui.midi_learn.process_cc(&mut s.song, 0, 74, 127));
        assert_eq!(s.song.midi_mappings.len(), 1);
        assert!((s.song.channels[0].cutoff - 16000.0).abs() < 5.0);
    }

    #[test]
    fn typing_piano_maps_two_octaves_and_routes_midi_notes() {
        assert_eq!(typing_key_to_midi(egui::Key::Z, 4), Some(60));
        assert_eq!(typing_key_to_midi(egui::Key::Q, 4), Some(72));
        assert_eq!(typing_key_to_midi(egui::Key::P, 4), Some(88));
        assert_eq!(typing_key_to_midi(egui::Key::F1, 4), None);

        let mut st = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        st.ui.typing_piano = true;
        st.ui.sel_ch = 4;
        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                process_typing_piano(&mut st.ui, &mut st.app, u.ctx());
            },
            st,
        );
        h.step();
        h.event(egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
        assert_eq!(h.state().ui.held_typing_keys.len(), 1);
        assert_eq!(h.state().ui.held_typing_keys[0].1, 60);
        h.event(egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
        assert!(h.state().ui.held_typing_keys.is_empty());
    }

    #[test]
    fn workspace_shortcuts_respect_panel_focus_and_toggle_dock() {
        let mut st = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        st.app.song.patterns[0].notes[4].push(Note::new(0, 4, 60, 0.9));
        st.app.song.patterns[0].notes[4].push(Note::new(16, 4, 64, 0.9));
        st.ui.sel_ch = 4;
        st.ui.sel_insert = 3;
        st.ui.focus = FocusPanel::PianoRoll;

        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                handle_workspace_shortcuts(&mut st.ui, &mut st.app, u.ctx());
            },
            st,
        );
        h.step();

        // Ctrl+L in PianoRoll triggers legato (extends note 0 up to tick 16).
        h.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::L);
        h.step();
        assert_eq!(h.state().app.song.patterns[0].notes[4][0].len, 16);

        // Switch focus to Rack: Ctrl+L links selected channel to selected insert.
        h.state_mut().ui.focus = FocusPanel::Rack;
        h.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::L);
        h.step();
        assert_eq!(h.state().app.song.channels[4].mixer_insert, 3);

        // Ctrl+T toggles typing piano; F9 toggles Mixer dock tab.
        let before_mix = h.state().ui.show_mixer;
        h.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::T);
        h.key_press(egui::Key::F9);
        h.step();
        assert!(h.state().ui.typing_piano);
        assert_ne!(h.state().ui.show_mixer, before_mix);
        assert_eq!(
            h.state().ui.dock_state.find_tab(&DockTab::Mixer).is_some(),
            h.state().ui.show_mixer
        );
    }

    #[test]
    fn browser_filter_search_favs_and_dnd_helpers() {
        let mut prefs = BrowserPrefs::default();
        prefs.favorites.push("/tmp/kick_808.wav".into());
        assert!(matches_browser_filter(
            "kick_808.wav",
            "/tmp/kick_808.wav",
            "808",
            BrowserCategory::Drums,
            true,
            &prefs
        ));
        assert!(!matches_browser_filter(
            "pad_warm.wav",
            "/tmp/pad_warm.wav",
            "",
            BrowserCategory::Drums,
            false,
            &prefs
        ));
        assert!(!matches_browser_filter(
            "snare.wav",
            "/tmp/snare.wav",
            "",
            BrowserCategory::All,
            true,
            &prefs
        ));

        // Create temporary tiny WAV file for real DnD payload drop checks.
        let dir = std::env::temp_dir().join("thunder_ui_dnd_test");
        std::fs::create_dir_all(&dir).unwrap();
        let wav_path = dir.join("drop_kick.wav");
        crate::export::write_wav_file(
            &wav_path.to_string_lossy(),
            &vec![0.3; 512],
            &vec![0.3; 512],
            44100.0,
        )
        .unwrap();

        let mut ui = AppUi::default();
        let mut app = AppState::new();
        let sample_payload =
            BrowserDragPayload::SamplePath(wav_path.to_string_lossy().into_owned());

        // Drop onto new channel & existing Playlist track.
        let old_ch_len = app.song.channels.len();
        assert!(drop_browser_onto_channel(&mut ui, &mut app, None, &sample_payload).is_some());
        assert_eq!(app.song.channels.len(), old_ch_len + 1);
        assert!(drop_browser_onto_playlist(&mut ui, &mut app, 0, 16, &sample_payload).is_some());
        assert!(
            app.song
                .clips
                .iter()
                .any(|c| matches!(c.kind, ClipKind::Audio { .. }))
        );

        // Drop Factory Preset onto channel 0 & Stock FX onto Mixer insert 1.
        assert!(
            drop_browser_onto_channel(
                &mut ui,
                &mut app,
                Some(0),
                &BrowserDragPayload::PresetIdx(0)
            )
            .is_some()
        );
        assert!(drop_browser_onto_fx(
            &mut ui,
            &mut app,
            1,
            &BrowserDragPayload::FxChoice(0)
        ));
        assert_eq!(app.song.mixer_tracks[1].fx.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_modal_and_recovery_prompt_work() {
        let mut st = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        st.ui.show_export_modal = true;
        st.ui.show_recovery_prompt = true;

        let mut h = Harness::new_ui_state(
            |u: &mut egui::Ui, st: &mut TestState| {
                export_modal_ui(&mut st.ui, &mut st.app, u);
                u.separator();
                recovery_modal_ui(&mut st.ui, &mut st.app, u);
            },
            st,
        );
        h.set_size(egui::vec2(720.0, 560.0));
        h.step();

        h.get_by_label("FLAC 16-bit").click();
        h.get_by_label("Wrap remainder").click();
        h.get_by_label("48 kHz").click();
        h.get_by_label("2x").click();
        h.get_by_label("Discard Autosave").click();
        h.step();

        assert_eq!(h.state().ui.export_cfg.format, ExportFormat::Flac16);
        assert_eq!(h.state().ui.export_cfg.tail_mode, TailMode::Wrap);
        assert_eq!(h.state().ui.export_cfg.sample_rate, 48000);
        assert_eq!(h.state().ui.export_cfg.oversample, 2);
        assert!(!h.state().ui.show_recovery_prompt);
    }

    #[test]
    fn phase7_clap_browser_filter_drop_and_missing_error() {
        let prefs = BrowserPrefs::default();
        assert!(matches_browser_filter(
            "Test Synth",
            "clap:org.test.synth",
            "",
            BrowserCategory::Plugins,
            false,
            &prefs,
        ));
        assert!(!matches_browser_filter(
            "Test Synth",
            "clap:org.test.synth",
            "",
            BrowserCategory::Presets,
            false,
            &prefs,
        ));

        let mut ui = AppUi::default();
        let mut app = AppState::new();
        ui.clap_catalog.plugins.push(ClapPluginDesc {
            id: "org.test.missing".into(),
            name: "Missing CLAP".into(),
            vendor: "Vendor".into(),
            version: "1.0".into(),
            path: "/nonexistent_thunder_clap_12345.clap".into(),
            is_instrument: true,
            is_effect: true,
            has_note_in: true,
            has_audio_in: true,
            has_audio_out: true,
            features: vec!["instrument".into()],
        });

        let ch =
            drop_browser_onto_channel(&mut ui, &mut app, None, &BrowserDragPayload::ClapPlugin(0))
                .expect("drop onto new CLAP channel");
        let ch_rt_id = match &app.song.channels[ch].kind {
            ChannelKind::ClapPlugin(c) => c.rt_id,
            _ => panic!("expected ClapPlugin channel"),
        };
        assert!(
            ui.clap
                .slots
                .get(&ch_rt_id)
                .and_then(|s| s.load_error.as_ref())
                .is_some()
        );

        assert!(drop_browser_onto_fx(
            &mut ui,
            &mut app,
            1,
            &BrowserDragPayload::ClapPlugin(0),
        ));
        let fx_rt_id = match &app.song.mixer_tracks[1].fx[0].kind {
            FxKind::ClapPlugin(c) => c.rt_id,
            _ => panic!("expected ClapPlugin FX"),
        };
        assert!(
            ui.clap
                .slots
                .get(&fx_rt_id)
                .and_then(|s| s.load_error.as_ref())
                .is_some()
        );
    }

    #[test]
    fn piano_erase_tool_hover_does_not_delete_note() {
        let mut state = TestState {
            ui: AppUi::default(),
            app: AppState::new(),
            rect: None,
        };
        state.ui.tool = PianoTool::Erase;
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

        let pos = cell_center(&h, 8, 60);
        h.hover_at(pos);
        h.step();
        assert_eq!(
            h.state().app.song.patterns[0].notes[4].len(),
            1,
            "hovering with Erase tool must not delete notes"
        );

        click_at(&mut h, pos);
        assert!(
            h.state().app.song.patterns[0].notes[4].is_empty(),
            "clicking with Erase tool must delete the note"
        );
    }

    #[test]
    fn focus_dock_tab_selects_active_tab() {
        let mut dock = default_dock_state();
        focus_dock_tab(&mut dock, DockTab::Playlist);
        assert_eq!(
            dock.find_active_focused().map(|(_, t)| *t),
            Some(DockTab::Playlist)
        );
        focus_dock_tab(&mut dock, DockTab::PianoRoll);
        assert_eq!(
            dock.find_active_focused().map(|(_, t)| *t),
            Some(DockTab::PianoRoll)
        );
    }
}
