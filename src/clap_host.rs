//! CLAP plugin host via `clack-host` and `clack-extensions`:
//! cross-platform discovery, crash-safe subprocess scanning (`--scan-clap`),
//! lock-free audio processing, sample-accurate events/polyphonic expression,
//! parameter & state serialization, latency reporting, and native GUI windows.

use clack_extensions::audio_ports::{
    AudioPortRescanFlags, HostAudioPorts, HostAudioPortsImpl, PluginAudioPorts,
};
use clack_extensions::gui::{
    GuiApiType, GuiConfiguration, GuiSize, HostGui, HostGuiImpl, PluginGui, Window,
};
use clack_extensions::latency::{HostLatency, HostLatencyImpl, PluginLatency};
use clack_extensions::note_ports::{
    HostNotePorts, HostNotePortsImpl, NoteDialects, NotePortRescanFlags, PluginNotePorts,
};
use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamInfoFlags, ParamRescanFlags, PluginParams,
};
use clack_extensions::state::{HostState, HostStateImpl, PluginState};
use clack_host::events::event_types::{
    NoteExpressionEvent, NoteExpressionType, NoteOffEvent, NoteOnEvent, ParamValueEvent,
    TransportEvent, TransportFlags,
};
use clack_host::events::io::EventBuffer;
use clack_host::events::{EventFlags, EventHeader, Match, Pckn};
use clack_host::prelude::*;
use clack_host::utils::{BeatTime, SecondsTime};
use raw_window_handle::RawWindowHandle;
use serde::{Deserialize, Serialize};
use std::cell::OnceCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_CLAP_BLOCK: usize = 4096;
const SCAN_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Catalog & Discovery
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClapPluginDesc {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub path: String,
    pub is_instrument: bool,
    pub is_effect: bool,
    pub has_note_in: bool,
    pub has_audio_in: bool,
    pub has_audio_out: bool,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClapCatalog {
    pub plugins: Vec<ClapPluginDesc>,
    pub last_errors: Vec<String>,
}

#[derive(Serialize, Deserialize)]
enum ScanCliResult {
    Ok(Vec<ClapPluginDesc>),
    Err(String),
}

pub fn default_clap_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(env_paths) = std::env::var_os("CLAP_PATH") {
        for p in std::env::split_paths(&env_paths) {
            if !p.as_os_str().is_empty() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        for p in ["/usr/lib/clap", "/usr/local/lib/clap"] {
            let pb = PathBuf::from(p);
            if !out.contains(&pb) {
                out.push(pb);
            }
        }
        if let Some(h) = dirs::home_dir() {
            let p = h.join(".clap");
            if !out.contains(&p) {
                out.push(p);
            }
        }
        if let Some(d) = dirs::data_local_dir() {
            let p = d.join("clap");
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let sys = PathBuf::from("/Library/Audio/Plug-Ins/CLAP");
        if !out.contains(&sys) {
            out.push(sys);
        }
        if let Some(h) = dirs::home_dir() {
            let user = h.join("Library/Audio/Plug-Ins/CLAP");
            if !out.contains(&user) {
                out.push(user);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(common) = std::env::var_os("CommonProgramFiles") {
            let p = PathBuf::from(common).join("CLAP");
            if !out.contains(&p) {
                out.push(p);
            }
        }
        if let Some(d) = dirs::data_local_dir() {
            let p = d.join("Programs").join("Common").join("CLAP");
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

pub fn collect_clap_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("clap"))
            {
                if !out.contains(&p) {
                    out.push(p);
                }
            } else if p.is_dir() {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for root in roots {
        if root.exists() {
            walk(root, 0, &mut out);
        }
    }
    out.sort();
    out
}

pub fn clap_cache_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("thunder").join("clap_cache.json"))
}

pub fn load_cached_catalog() -> ClapCatalog {
    let Some(path) = clap_cache_path() else {
        return ClapCatalog::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<ClapCatalog>(&s).ok())
        .unwrap_or_default()
}

pub fn save_cached_catalog(cat: &ClapCatalog) {
    let Some(path) = clap_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cat) {
        let _ = std::fs::write(path, json);
    }
}

pub fn scan_clap_entry(path: &Path) -> Result<Vec<ClapPluginDesc>, String> {
    let entry = unsafe { PluginEntry::load(path) }.map_err(|e| format!("{e}"))?;
    inspect_entry_plugins(&entry, &path.to_string_lossy())
}

pub fn inspect_entry_plugins(
    entry: &PluginEntry,
    path_str: &str,
) -> Result<Vec<ClapPluginDesc>, String> {
    let Some(factory) = entry.get_plugin_factory() else {
        return Err("missing CLAP plugin factory".into());
    };
    let host_info = thunder_host_info()?;
    let mut out = Vec::new();
    for desc in factory.plugin_descriptors() {
        let Some(id_c) = desc.id() else {
            continue;
        };
        let id = id_c.to_string_lossy().into_owned();
        let name = desc
            .name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| id.clone());
        let vendor = desc
            .vendor()
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        let version = desc
            .version()
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        let features: Vec<String> = desc
            .features()
            .map(|f| f.to_string_lossy().into_owned())
            .collect();

        let (mut has_note_in, mut has_audio_in, mut has_audio_out) = (false, false, true);
        if let Ok(mut inst) = PluginInstance::<ThunderClapHost>::new(
            |_| ThunderHostShared::default(),
            |shared| ThunderHostMain::new(shared),
            entry,
            id_c,
            &host_info,
        ) {
            let ap = inst.access_shared_handler(|s| *s.audio_ports.get().unwrap_or(&None));
            let np = inst.access_shared_handler(|s| *s.note_ports.get().unwrap_or(&None));
            let h = inst.plugin_handle();
            if let Some(ap) = ap {
                has_audio_in = ap.count(&h, true) > 0;
                has_audio_out = ap.count(&h, false) > 0;
            }
            if let Some(np) = np {
                has_note_in = np.count(&h, true) > 0;
            }
        }

        let feat_inst = features.iter().any(|f| {
            matches!(
                f.as_str(),
                "instrument" | "synthesizer" | "sampler" | "drum-machine" | "drum"
            )
        });
        let feat_fx = features.iter().any(|f| f == "audio-effect");
        let is_instrument = feat_inst || (has_note_in && !has_audio_in);
        let is_effect = feat_fx || has_audio_in || !is_instrument;

        out.push(ClapPluginDesc {
            id,
            name,
            vendor,
            version,
            path: path_str.to_string(),
            is_instrument,
            is_effect,
            has_note_in,
            has_audio_in,
            has_audio_out,
            features,
        });
    }
    Ok(out)
}

/// Entrypoint for `--scan-clap <path>` subprocess mode.
pub fn run_scan_cli(path: &str) -> i32 {
    let res = match scan_clap_entry(Path::new(path)) {
        Ok(list) => ScanCliResult::Ok(list),
        Err(e) => ScanCliResult::Err(e),
    };
    if let Ok(json) = serde_json::to_string(&res) {
        println!("{json}");
    }
    match res {
        ScanCliResult::Ok(_) => 0,
        ScanCliResult::Err(_) => 1,
    }
}

/// Spawn `thunder --scan-clap <file>` per bundle so buggy plugins cannot crash the DAW.
pub fn scan_clap_file_isolated(path: &Path) -> Result<Vec<ClapPluginDesc>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut child = Command::new(exe)
        .arg("--scan-clap")
        .arg(path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn scanner: {e}"))?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if start.elapsed() > SCAN_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out scanning {}", path.display()));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(format!("wait error: {e}")),
        }
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("scanner output: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "scanner exited with {} for {}",
            out.status,
            path.display()
        ));
    }
    match serde_json::from_str::<ScanCliResult>(trimmed) {
        Ok(ScanCliResult::Ok(list)) => Ok(list),
        Ok(ScanCliResult::Err(e)) => Err(e),
        Err(e) => Err(format!("malformed scanner JSON: {e}")),
    }
}

pub fn scan_clap_dirs(dirs: &[PathBuf]) -> ClapCatalog {
    let files = collect_clap_files(dirs);
    let mut cat = ClapCatalog::default();
    for file in files {
        match scan_clap_file_isolated(&file) {
            Ok(list) => cat.plugins.extend(list),
            Err(e) => cat.last_errors.push(format!("{}: {e}", file.display())),
        }
    }
    cat.plugins.sort_by_key(|a| a.name.to_lowercase());
    cat
}

// ---------------------------------------------------------------------------
// Base64 helper (zero-dep compact state serialization into Song JSON)
// ---------------------------------------------------------------------------

const B64_ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode_b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let v = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHA[((v >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHA[((v >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHA[((v >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHA[(v & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn decode_b64(text: &str) -> Result<Vec<u8>, String> {
    fn val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return Err("invalid base64 length".into());
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.as_chunks::<4>().0 {
        let v0 = val(chunk[0]).ok_or_else(|| "invalid base64 char".to_string())?;
        let v1 = val(chunk[1]).ok_or_else(|| "invalid base64 char".to_string())?;
        let pad2 = chunk[2] == b'=';
        let pad3 = chunk[3] == b'=';
        let v2 = if pad2 {
            0
        } else {
            val(chunk[2]).ok_or_else(|| "invalid base64 char".to_string())?
        };
        let v3 = if pad3 {
            0
        } else {
            val(chunk[3]).ok_or_else(|| "invalid base64 char".to_string())?
        };
        let n = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
        out.push((n >> 16) as u8);
        if !pad2 {
            out.push(((n >> 8) & 0xFF) as u8);
        }
        if !pad3 {
            out.push((n & 0xFF) as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Host Handlers
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct ThunderHostShared {
    callback_requested: AtomicBool,
    latency_changed: AtomicBool,
    params_changed: AtomicBool,
    state_dirty: AtomicBool,
    gui_closed: AtomicBool,
    gui_destroyed: AtomicBool,
    latency: OnceLock<Option<PluginLatency>>,
    state: OnceLock<Option<PluginState>>,
    params: OnceLock<Option<PluginParams>>,
    gui: OnceLock<Option<PluginGui>>,
    audio_ports: OnceLock<Option<PluginAudioPorts>>,
    note_ports: OnceLock<Option<PluginNotePorts>>,
}

impl<'a> SharedHandler<'a> for ThunderHostShared {
    fn initializing(&self, instance: InitializingPluginHandle<'a>) {
        let _ = self.latency.set(instance.get_extension());
        let _ = self.state.set(instance.get_extension());
        let _ = self.params.set(instance.get_extension());
        let _ = self.gui.set(instance.get_extension());
        let _ = self.audio_ports.set(instance.get_extension());
        let _ = self.note_ports.set(instance.get_extension());
    }

    fn request_restart(&self) {
        self.callback_requested.store(true, Ordering::Release);
    }

    fn request_process(&self) {}

    fn request_callback(&self) {
        self.callback_requested.store(true, Ordering::Release);
    }
}

impl HostParamsImplShared for ThunderHostShared {
    fn request_flush(&self) {}
}

impl HostGuiImpl for ThunderHostShared {
    fn resize_hints_changed(&self) {}

    fn request_resize(&self, _new_size: GuiSize) -> Result<(), HostError> {
        Ok(())
    }

    fn request_show(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn request_hide(&self) -> Result<(), HostError> {
        self.gui_closed.store(true, Ordering::Release);
        Ok(())
    }

    fn closed(&self, was_destroyed: bool) {
        self.gui_closed.store(true, Ordering::Release);
        if was_destroyed {
            self.gui_destroyed.store(true, Ordering::Release);
        }
    }
}

pub struct ThunderHostMain<'a> {
    shared: &'a ThunderHostShared,
    handle: OnceCell<InitializedPluginHandle<'a>>,
}

impl<'a> ThunderHostMain<'a> {
    fn new(shared: &'a ThunderHostShared) -> Self {
        Self {
            shared,
            handle: OnceCell::new(),
        }
    }
}

impl<'a> MainThreadHandler<'a> for ThunderHostMain<'a> {
    fn initialized(&self, instance: InitializedPluginHandle<'a>) {
        let _ = self.handle.set(instance);
    }
}

impl HostLatencyImpl for ThunderHostMain<'_> {
    fn changed(&self) {
        self.shared.latency_changed.store(true, Ordering::Release);
    }
}

impl HostStateImpl for ThunderHostMain<'_> {
    fn mark_dirty(&self) {
        self.shared.state_dirty.store(true, Ordering::Release);
    }
}

impl HostParamsImplMainThread for ThunderHostMain<'_> {
    fn rescan(&self, flags: ParamRescanFlags) {
        if flags
            .intersects(ParamRescanFlags::VALUES | ParamRescanFlags::INFO | ParamRescanFlags::TEXT)
        {
            self.shared.params_changed.store(true, Ordering::Release);
        }
    }

    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

impl HostAudioPortsImpl for ThunderHostMain<'_> {
    fn is_rescan_flag_supported(&self, _flag: AudioPortRescanFlags) -> bool {
        false
    }

    fn rescan(&self, _flags: AudioPortRescanFlags) {}
}

impl HostNotePortsImpl for ThunderHostMain<'_> {
    fn supported_dialects(&self) -> NoteDialects {
        NoteDialects::CLAP | NoteDialects::MIDI
    }

    fn rescan(&self, _flags: NotePortRescanFlags) {}
}

pub struct ThunderClapHost;

impl HostHandlers for ThunderClapHost {
    type Shared<'a> = ThunderHostShared;
    type MainThread<'a> = ThunderHostMain<'a>;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder
            .register::<HostLatency>()
            .register::<HostState>()
            .register::<HostParams>()
            .register::<HostGui>()
            .register::<HostAudioPorts>()
            .register::<HostNotePorts>();
    }
}

pub fn thunder_host_info() -> Result<HostInfo, String> {
    HostInfo::new(
        "Thunder",
        "Cotanjant",
        "https://cotanjant.github.io/thunder/",
        env!("CARGO_PKG_VERSION"),
    )
    .map_err(|e| format!("{e}"))
}

// ---------------------------------------------------------------------------
// Audio-Thread Processor Node (`Send`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct ClapBlockNote {
    pub sample_offset: u32,
    pub key: u8,
    pub vel: f32,
    pub pan: f32,
    pub fine_pitch: f32,
    pub dur_samples: usize,
}

#[derive(Clone, Copy)]
enum QueuedMidi {
    NoteOn { key: u8, vel: f32 },
    NoteOff { key: u8 },
}

pub struct ClapAudioNode {
    proc: Option<StartedPluginAudioProcessor<ThunderClapHost>>,
    sr: f32,
    steady_time: u64,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    in_events: EventBuffer,
    out_events: EventBuffer,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    pub out_l: Vec<f32>,
    pub out_r: Vec<f32>,
    active_notes: Vec<(u8, usize)>,
    live_queue: Vec<QueuedMidi>,
    param_queue: Vec<(u32, f64)>,
}

impl Drop for ClapAudioNode {
    fn drop(&mut self) {
        if let Some(p) = self.proc.take() {
            drop(p.stop_processing());
        }
    }
}

impl ClapAudioNode {
    fn new(proc: StartedPluginAudioProcessor<ThunderClapHost>, sr: f32) -> Self {
        Self {
            proc: Some(proc),
            sr: sr.max(1.0),
            steady_time: 0,
            in_ports: AudioPorts::with_capacity(2, 1),
            out_ports: AudioPorts::with_capacity(2, 1),
            in_events: EventBuffer::with_capacity(256),
            out_events: EventBuffer::with_capacity(64),
            in_l: vec![0.0; MAX_CLAP_BLOCK],
            in_r: vec![0.0; MAX_CLAP_BLOCK],
            out_l: vec![0.0; MAX_CLAP_BLOCK],
            out_r: vec![0.0; MAX_CLAP_BLOCK],
            active_notes: Vec::with_capacity(64),
            live_queue: Vec::with_capacity(64),
            param_queue: Vec::with_capacity(64),
        }
    }

    pub fn queue_param(&mut self, param_id: u32, value: f64) {
        if self.param_queue.len() < 256 {
            self.param_queue.push((param_id, value));
        }
    }

    pub fn queue_live_note_on(&mut self, key: u8, vel: f32) {
        if self.live_queue.len() < 128 {
            self.live_queue.push(QueuedMidi::NoteOn { key, vel });
        }
    }

    pub fn queue_live_note_off(&mut self, key: u8) {
        if self.live_queue.len() < 128 {
            self.live_queue.push(QueuedMidi::NoteOff { key });
        }
    }

    pub fn queue_live_preview(&mut self, key: u8, vel: f32, dur_samples: usize) {
        self.queue_live_note_on(key, vel);
        if self.active_notes.len() < 128 {
            self.active_notes.push((key, dur_samples.max(1)));
        }
    }

    pub fn reset(&mut self) {
        self.active_notes.clear();
        self.live_queue.clear();
        if let Some(p) = self.proc.as_mut() {
            p.reset();
        }
    }

    fn build_transport(
        bpm: f32,
        playing: bool,
        tick_pos: f64,
        sr: f32,
        sample_pos: u64,
    ) -> TransportEvent {
        let mut flags = TransportFlags::HAS_TEMPO
            | TransportFlags::HAS_BEATS_TIMELINE
            | TransportFlags::HAS_SECONDS_TIMELINE
            | TransportFlags::HAS_TIME_SIGNATURE;
        if playing {
            flags |= TransportFlags::IS_PLAYING;
        }
        let beats = tick_pos / 4.0;
        let secs = sample_pos as f64 / sr.max(1.0) as f64;
        TransportEvent {
            header: EventHeader::new_core(0, EventFlags::empty()),
            flags,
            song_pos_beats: BeatTime::from_float(beats),
            song_pos_seconds: SecondsTime::from_float(secs),
            tempo: bpm.max(30.0) as f64,
            tempo_inc: 0.0,
            loop_start_beats: BeatTime::from_int(0),
            loop_end_beats: BeatTime::from_int(4),
            loop_start_seconds: SecondsTime::from_int(0),
            loop_end_seconds: SecondsTime::from_int(2),
            bar_start: BeatTime::from_int((beats.floor() as i64 / 4) * 4),
            bar_number: (beats / 4.0).floor() as i32,
            time_signature_numerator: 4,
            time_signature_denominator: 4,
        }
    }

    fn drain_param_events(&mut self) {
        for (pid, val) in self.param_queue.drain(..) {
            if let Some(id) = ClapId::from_raw(pid) {
                let ev = ParamValueEvent::new(0, id, Pckn::match_all(), val);
                self.in_events.push(&ev);
            }
        }
    }

    pub fn process_instrument_block(
        &mut self,
        frames: usize,
        bpm: f32,
        playing: bool,
        sample_pos: u64,
        tick_pos: f64,
        notes: &[ClapBlockNote],
    ) {
        if self.out_l.len() < frames {
            self.out_l.resize(frames, 0.0);
            self.out_r.resize(frames, 0.0);
        }
        self.out_l[..frames].fill(0.0);
        self.out_r[..frames].fill(0.0);

        let Some(mut proc) = self.proc.take() else {
            return;
        };

        let mut offset = 0usize;
        while offset < frames {
            let chunk = (frames - offset).min(MAX_CLAP_BLOCK);
            self.in_events.clear();
            self.out_events.clear();
            if offset == 0 {
                self.drain_param_events();
                for qm in self.live_queue.drain(..) {
                    match qm {
                        QueuedMidi::NoteOn { key, vel } => {
                            let ev = NoteOnEvent::new(
                                0,
                                Pckn::new(0u16, 0u16, key, Match::All),
                                vel as f64,
                            );
                            self.in_events.push(&ev);
                        }
                        QueuedMidi::NoteOff { key } => {
                            let ev =
                                NoteOffEvent::new(0, Pckn::new(0u16, 0u16, key, Match::All), 0.0);
                            self.in_events.push(&ev);
                        }
                    }
                }
            }

            // Advance active note-offs that expire inside this chunk.
            let mut ai = 0usize;
            while ai < self.active_notes.len() {
                let (key, rem) = self.active_notes[ai];
                if rem < chunk {
                    let ev =
                        NoteOffEvent::new(rem as u32, Pckn::new(0u16, 0u16, key, Match::All), 0.0);
                    self.in_events.push(&ev);
                    self.active_notes.swap_remove(ai);
                } else {
                    self.active_notes[ai].1 = rem - chunk;
                    ai += 1;
                }
            }

            for bn in notes {
                let so = bn.sample_offset as usize;
                if so >= offset && so < offset + chunk {
                    let rel = (so - offset) as u32;
                    let pckn = Pckn::new(0u16, 0u16, bn.key, Match::All);
                    let on = NoteOnEvent::new(rel, pckn, bn.vel.clamp(0.01, 1.0) as f64);
                    self.in_events.push(&on);
                    if bn.pan.abs() > 0.001 {
                        let pe = NoteExpressionEvent::new(
                            rel,
                            pckn,
                            NoteExpressionType::Pan,
                            ((bn.pan + 1.0) * 0.5).clamp(0.0, 1.0) as f64,
                        );
                        self.in_events.push(&pe);
                    }
                    if bn.fine_pitch.abs() > 0.001 {
                        let te = NoteExpressionEvent::new(
                            rel,
                            pckn,
                            NoteExpressionType::Tuning,
                            bn.fine_pitch.clamp(-120.0, 120.0) as f64,
                        );
                        self.in_events.push(&te);
                    }
                    let end_rel = rel as usize + bn.dur_samples.max(1);
                    if end_rel < chunk {
                        let off = NoteOffEvent::new(end_rel as u32, pckn, 0.0);
                        self.in_events.push(&off);
                    } else {
                        let rem = end_rel - chunk;
                        if self.active_notes.len() < 128 {
                            self.active_notes.push((bn.key, rem));
                        }
                    }
                }
            }

            self.in_events.sort();
            self.in_l[..chunk].fill(0.0);
            self.in_r[..chunk].fill(0.0);
            let out_slice_l = &mut self.out_l[offset..offset + chunk];
            let out_slice_r = &mut self.out_r[offset..offset + chunk];

            let ins = self.in_ports.with_input_buffers([AudioPortBuffer {
                channels: AudioPortBufferType::f32_input_only([
                    InputChannel::constant(&mut self.in_l[..chunk]),
                    InputChannel::constant(&mut self.in_r[..chunk]),
                ]),
                latency: 0,
            }]);
            let mut outs = self.out_ports.with_output_buffers([AudioPortBuffer {
                channels: AudioPortBufferType::f32_output_only([out_slice_l, out_slice_r]),
                latency: 0,
            }]);

            let in_ev = self.in_events.as_input();
            let mut out_ev = self.out_events.as_output();
            let tr =
                Self::build_transport(bpm, playing, tick_pos, self.sr, sample_pos + offset as u64);
            let _ = proc.process(
                &ins,
                &mut outs,
                &in_ev,
                &mut out_ev,
                Some(self.steady_time),
                Some(&tr),
            );
            self.steady_time = self.steady_time.wrapping_add(chunk as u64);
            offset += chunk;
        }
        self.proc = Some(proc);
    }

    pub fn process_fx_frame(
        &mut self,
        in_l: f32,
        in_r: f32,
        bpm: f32,
        playing: bool,
    ) -> (f32, f32) {
        let Some(mut proc) = self.proc.take() else {
            return (in_l, in_r);
        };
        self.in_events.clear();
        self.out_events.clear();
        self.drain_param_events();
        self.in_events.sort();

        self.in_l[0] = in_l;
        self.in_r[0] = in_r;
        self.out_l[0] = 0.0;
        self.out_r[0] = 0.0;

        let ins = self.in_ports.with_input_buffers([AudioPortBuffer {
            channels: AudioPortBufferType::f32_input_only([
                InputChannel::variable(&mut self.in_l[..1]),
                InputChannel::variable(&mut self.in_r[..1]),
            ]),
            latency: 0,
        }]);
        let mut outs = self.out_ports.with_output_buffers([AudioPortBuffer {
            channels: AudioPortBufferType::f32_output_only([
                &mut self.out_l[..1],
                &mut self.out_r[..1],
            ]),
            latency: 0,
        }]);
        let in_ev = self.in_events.as_input();
        let mut out_ev = self.out_events.as_output();
        let tr = Self::build_transport(bpm, playing, 0.0, self.sr, self.steady_time);
        let ok = proc
            .process(
                &ins,
                &mut outs,
                &in_ev,
                &mut out_ev,
                Some(self.steady_time),
                Some(&tr),
            )
            .is_ok();
        self.steady_time = self.steady_time.wrapping_add(1);
        self.proc = Some(proc);
        if ok {
            (self.out_l[0], self.out_r[0])
        } else {
            (in_l, in_r)
        }
    }
}

// ---------------------------------------------------------------------------
// Main-Thread Instance + Manager (`!Send`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ClapParamInfo {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub value: f64,
    pub stepped: bool,
    pub readonly: bool,
    pub hidden: bool,
    pub text: String,
}

pub struct ClapMainInstance {
    instance: PluginInstance<ThunderClapHost>,
    pub params: Vec<ClapParamInfo>,
    pub latency_samples: usize,
    pub gui_created: bool,
    pub gui_open: bool,
    pub gui_error: Option<String>,
}

impl Drop for ClapMainInstance {
    fn drop(&mut self) {
        self.close_gui();
        let _ = self.instance.try_deactivate();
    }
}

impl ClapMainInstance {
    pub fn instantiate_from_path(
        path: &str,
        plugin_id: &str,
        state_b64: &str,
        param_overrides: &[(u32, f64)],
        sr: f32,
    ) -> Result<(Self, ClapAudioNode), String> {
        let entry = unsafe { PluginEntry::load(Path::new(path)) }.map_err(|e| format!("{e}"))?;
        Self::instantiate_from_entry(entry, plugin_id, state_b64, param_overrides, sr)
    }

    pub fn instantiate_from_entry(
        entry: PluginEntry,
        plugin_id: &str,
        state_b64: &str,
        param_overrides: &[(u32, f64)],
        sr: f32,
    ) -> Result<(Self, ClapAudioNode), String> {
        let id_c = CString::new(plugin_id).map_err(|e| format!("invalid plugin id: {e}"))?;
        let host_info = thunder_host_info()?;
        let instance = PluginInstance::<ThunderClapHost>::new(
            |_| ThunderHostShared::default(),
            |shared| ThunderHostMain::new(shared),
            &entry,
            &id_c,
            &host_info,
        )
        .map_err(|e| format!("{e}"))?;

        let mut main = Self {
            instance,
            params: Vec::new(),
            latency_samples: 0,
            gui_created: false,
            gui_open: false,
            gui_error: None,
        };
        if !state_b64.is_empty() {
            let _ = main.load_state_b64(state_b64);
        }
        main.refresh_params();
        main.refresh_latency();

        let cfg = PluginAudioConfiguration {
            sample_rate: sr.max(8000.0) as f64,
            min_frames_count: 1,
            max_frames_count: MAX_CLAP_BLOCK as u32,
        };
        let stopped = main
            .instance
            .activate(|_, _| (), cfg)
            .map_err(|e| format!("activate failed: {e}"))?;
        let started = stopped
            .start_processing()
            .map_err(|e| format!("start_processing failed: {e}"))?;
        main.refresh_latency();

        let mut node = ClapAudioNode::new(started, sr);
        for &(pid, val) in param_overrides {
            node.queue_param(pid, val);
            if let Some(p) = main.params.iter_mut().find(|p| p.id == pid) {
                p.value = val.clamp(p.min, p.max);
            }
        }
        Ok((main, node))
    }

    pub fn save_state_b64(&mut self) -> Result<String, String> {
        let Some(state_ext) = self
            .instance
            .access_shared_handler(|s| *s.state.get().unwrap_or(&None))
        else {
            return Err("plugin does not implement clap.state".into());
        };
        let mut buf = Vec::new();
        let h = self.instance.plugin_handle();
        state_ext.save(&h, &mut buf).map_err(|e| format!("{e}"))?;
        Ok(encode_b64(&buf))
    }

    pub fn load_state_b64(&mut self, b64: &str) -> Result<(), String> {
        let Some(state_ext) = self
            .instance
            .access_shared_handler(|s| *s.state.get().unwrap_or(&None))
        else {
            return Err("plugin does not implement clap.state".into());
        };
        let bytes = decode_b64(b64)?;
        let mut cursor = std::io::Cursor::new(bytes);
        let h = self.instance.plugin_handle();
        state_ext
            .load(&h, &mut cursor)
            .map_err(|e| format!("{e}"))?;
        self.refresh_params();
        Ok(())
    }

    pub fn refresh_latency(&mut self) -> usize {
        let lat_ext = self
            .instance
            .access_shared_handler(|s| *s.latency.get().unwrap_or(&None));
        if let Some(lat) = lat_ext {
            let h = self.instance.plugin_handle();
            self.latency_samples = lat.get(&h) as usize;
        }
        self.latency_samples
    }

    pub fn refresh_params(&mut self) {
        self.params.clear();
        let Some(params_ext) = self
            .instance
            .access_shared_handler(|s| *s.params.get().unwrap_or(&None))
        else {
            return;
        };
        let h = self.instance.plugin_handle();
        let count = params_ext.count(&h);
        let mut buf = ParamInfoBuffer::new();
        let mut text_buf = [0u8; 64];
        for i in 0..count {
            let Some(info) = params_ext.get_info(&h, i, &mut buf) else {
                continue;
            };
            let val = params_ext
                .get_value(&h, info.id)
                .unwrap_or(info.default_value);
            let text = match params_ext.value_to_text(&h, info.id, val, &mut text_buf) {
                Ok(s) => String::from_utf8_lossy(s).into_owned(),
                Err(_) => format!("{val:.2}"),
            };
            self.params.push(ClapParamInfo {
                id: info.id.get(),
                name: String::from_utf8_lossy(info.name).into_owned(),
                module: String::from_utf8_lossy(info.module).into_owned(),
                min: info.min_value,
                max: info.max_value.max(info.min_value + 1e-6),
                default: info.default_value,
                value: val,
                stepped: info.flags.contains(ParamInfoFlags::IS_STEPPED),
                readonly: info.flags.contains(ParamInfoFlags::IS_READONLY),
                hidden: info.flags.contains(ParamInfoFlags::IS_HIDDEN),
                text,
            });
        }
    }

    /// Poll main-thread callback flags. Returns `(state_dirty, latency_or_params_changed)`.
    pub fn poll_callbacks(&mut self) -> (bool, bool) {
        let (cb, lat, prm, dirty, closed, destroyed) = self.instance.access_shared_handler(|s| {
            (
                s.callback_requested.swap(false, Ordering::AcqRel),
                s.latency_changed.swap(false, Ordering::AcqRel),
                s.params_changed.swap(false, Ordering::AcqRel),
                s.state_dirty.swap(false, Ordering::AcqRel),
                s.gui_closed.swap(false, Ordering::AcqRel),
                s.gui_destroyed.swap(false, Ordering::AcqRel),
            )
        });
        if cb {
            self.instance.call_on_main_thread_callback();
        }
        if lat {
            self.refresh_latency();
        }
        if prm {
            self.refresh_params();
        }
        if closed {
            self.gui_open = false;
        }
        if destroyed {
            self.close_gui();
        }
        (dirty, lat || prm)
    }

    pub fn open_gui(&mut self, parent: Option<RawWindowHandle>, title: &str) -> Result<(), String> {
        let Some(gui_ext) = self
            .instance
            .access_shared_handler(|s| *s.gui.get().unwrap_or(&None))
        else {
            return Err("plugin does not implement clap.gui".into());
        };
        if self.gui_created {
            let h = self.instance.plugin_handle();
            gui_ext.show(&h).map_err(|e| format!("{e}"))?;
            self.gui_open = true;
            self.gui_error = None;
            return Ok(());
        }

        let h = self.instance.plugin_handle();
        let preferred = gui_ext
            .get_preferred_api(&h)
            .map(|c| (c.api_type, c.is_floating));
        let platform_api = GuiApiType::default_for_current_platform();

        let mut candidates: Vec<(GuiApiType<'_>, bool)> = Vec::new();
        if let Some((api, flt)) = preferred {
            candidates.push((api, flt));
        }
        if let Some(api) = platform_api {
            candidates.push((api, true));
            candidates.push((api, false));
        }
        #[cfg(target_os = "linux")]
        {
            candidates.push((GuiApiType::WAYLAND, true));
        }

        let chosen = candidates
            .into_iter()
            .find(|&(api_type, is_floating)| {
                gui_ext.is_api_supported(
                    &h,
                    GuiConfiguration {
                        api_type,
                        is_floating,
                    },
                )
            })
            .ok_or_else(|| "no supported plugin GUI API".to_string())?;

        gui_ext
            .create(
                &h,
                GuiConfiguration {
                    api_type: chosen.0,
                    is_floating: chosen.1,
                },
            )
            .map_err(|e| format!("{e}"))?;

        if chosen.1 {
            if let Some(raw) = parent
                && let Some(win) = clap_window_from_rwh(raw)
            {
                let _ = unsafe { gui_ext.set_transient(&h, win) };
            }
            if let Ok(title_c) = CString::new(title) {
                gui_ext.suggest_title(&h, &title_c);
            }
        } else {
            let Some(raw) = parent else {
                gui_ext.destroy(&h);
                return Err("plugin GUI requires a native parent window".into());
            };
            let Some(win) = clap_window_from_rwh(raw) else {
                gui_ext.destroy(&h);
                return Err("unsupported native parent window handle for plugin GUI".into());
            };
            let _ = gui_ext.get_resize_hints(&h);
            let _ = gui_ext.set_scale(&h, 1.0);
            if let Err(e) = unsafe { gui_ext.set_parent(&h, win) } {
                gui_ext.destroy(&h);
                return Err(format!("{e}"));
            }
        }

        if let Err(e) = gui_ext.show(&h) {
            gui_ext.destroy(&h);
            return Err(format!("{e}"));
        }
        self.gui_created = true;
        self.gui_open = true;
        self.gui_error = None;
        Ok(())
    }

    pub fn close_gui(&mut self) {
        if !self.gui_created {
            return;
        }
        if let Some(gui_ext) = self
            .instance
            .access_shared_handler(|s| *s.gui.get().unwrap_or(&None))
        {
            let h = self.instance.plugin_handle();
            let _ = gui_ext.hide(&h);
            gui_ext.destroy(&h);
        }
        self.gui_created = false;
        self.gui_open = false;
    }
}

fn clap_window_from_rwh(raw: RawWindowHandle) -> Option<Window<'static, 'static>> {
    match raw {
        RawWindowHandle::Xlib(h) => Some(Window::from_x11_handle(h.window)),
        RawWindowHandle::Xcb(h) => Some(Window::from_x11_handle(h.window.get().into())),
        RawWindowHandle::Win32(h) => {
            Some(unsafe { Window::from_win32_hwnd(h.hwnd.get() as *mut _) })
        }
        RawWindowHandle::AppKit(h) => {
            Some(unsafe { Window::from_cocoa_nsview(h.ns_view.as_ptr()) })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Main-Thread Plugin Slot Manager
// ---------------------------------------------------------------------------

pub struct ClapSlotState {
    pub path: String,
    pub plugin_id: String,
    pub main: Option<ClapMainInstance>,
    pub load_error: Option<String>,
}

#[derive(Default)]
pub struct ClapManager {
    pub slots: HashMap<u64, ClapSlotState>,
    sr: f32,
}

impl ClapManager {
    pub fn ensure_slot(
        &mut self,
        cfg: &mut crate::song::ClapPluginCfg,
        sr: f32,
    ) -> Option<ClapAudioNode> {
        let sr = sr.max(8000.0);
        if (self.sr - sr).abs() > 1.0 {
            self.slots.clear();
            self.sr = sr;
        }
        let needs_load = match self.slots.get(&cfg.rt_id) {
            Some(s) => s.path != cfg.path || s.plugin_id != cfg.plugin_id,
            None => true,
        };
        if !needs_load {
            return None;
        }
        if cfg.path.is_empty() || cfg.plugin_id.is_empty() {
            return None;
        }
        match ClapMainInstance::instantiate_from_path(
            &cfg.path,
            &cfg.plugin_id,
            &cfg.state_b64,
            &cfg.params,
            sr,
        ) {
            Ok((main, node)) => {
                cfg.reported_latency = main.latency_samples;
                self.slots.insert(
                    cfg.rt_id,
                    ClapSlotState {
                        path: cfg.path.clone(),
                        plugin_id: cfg.plugin_id.clone(),
                        main: Some(main),
                        load_error: None,
                    },
                );
                Some(node)
            }
            Err(e) => {
                self.slots.insert(
                    cfg.rt_id,
                    ClapSlotState {
                        path: cfg.path.clone(),
                        plugin_id: cfg.plugin_id.clone(),
                        main: None,
                        load_error: Some(e),
                    },
                );
                None
            }
        }
    }

    pub fn reload_slot(
        &mut self,
        cfg: &mut crate::song::ClapPluginCfg,
        sr: f32,
    ) -> Option<ClapAudioNode> {
        self.slots.remove(&cfg.rt_id);
        self.ensure_slot(cfg, sr)
    }
}

/// Mount all valid `.clap` binaries referenced by `song` for offline rendering.
pub fn mount_all_for_offline(
    song: &crate::song::Song,
    sr: f32,
) -> (Vec<ClapMainInstance>, HashMap<u64, ClapAudioNode>) {
    let mut mains = Vec::new();
    let mut nodes = HashMap::new();
    for ch in &song.channels {
        if let crate::song::ChannelKind::ClapPlugin(cfg) = &ch.kind
            && !cfg.path.is_empty()
            && Path::new(&cfg.path).exists()
            && let Ok((main, node)) = ClapMainInstance::instantiate_from_path(
                &cfg.path,
                &cfg.plugin_id,
                &cfg.state_b64,
                &cfg.params,
                sr,
            )
        {
            nodes.insert(cfg.rt_id, node);
            mains.push(main);
        }
    }
    for track in &song.mixer_tracks {
        for slot in &track.fx {
            if let crate::song::FxKind::ClapPlugin(cfg) = &slot.kind
                && !cfg.path.is_empty()
                && Path::new(&cfg.path).exists()
                && let Ok((main, node)) = ClapMainInstance::instantiate_from_path(
                    &cfg.path,
                    &cfg.plugin_id,
                    &cfg.state_b64,
                    &cfg.params,
                    sr,
                )
            {
                nodes.insert(cfg.rt_id, node);
                mains.push(main);
            }
        }
    }
    (mains, nodes)
}

// ---------------------------------------------------------------------------
// Tests (including an in-process C-ABI CLAP stub plugin!)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_stub {
    use clack_host::entry::EntryDescriptor;
    use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
    use clap_sys::ext::params::{
        CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, clap_param_info, clap_plugin_params,
    };
    use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
    use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
    use clap_sys::host::clap_host;
    use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
    use clap_sys::process::{CLAP_PROCESS_CONTINUE, clap_process, clap_process_status};
    use clap_sys::stream::{clap_istream, clap_ostream};
    use clap_sys::version::CLAP_VERSION;
    use std::ffi::{CStr, c_char, c_void};
    use std::ptr::null;

    struct StubPluginState {
        gain: f64,
        gate: f64,
    }

    struct SyncFeatures([*const c_char; 3]);
    unsafe impl Sync for SyncFeatures {}

    const ID: &CStr = c"org.cotanjant.thunder.test_stub";
    const NAME: &CStr = c"Thunder Test CLAP";
    const VENDOR: &CStr = c"Cotanjant";
    const VER: &CStr = c"1.0";
    const FEAT_INST: &CStr = c"instrument";
    const FEAT_STEREO: &CStr = c"stereo";
    static FEATURES: SyncFeatures =
        SyncFeatures([FEAT_INST.as_ptr(), FEAT_STEREO.as_ptr(), null()]);

    static DESC: clap_plugin_descriptor = clap_plugin_descriptor {
        clap_version: CLAP_VERSION,
        id: ID.as_ptr(),
        name: NAME.as_ptr(),
        vendor: VENDOR.as_ptr(),
        url: null(),
        manual_url: null(),
        support_url: null(),
        version: VER.as_ptr(),
        description: NAME.as_ptr(),
        features: FEATURES.0.as_ptr(),
    };

    unsafe extern "C" fn lat_get(_p: *const clap_plugin) -> u32 {
        32
    }
    static LAT_EXT: clap_plugin_latency = clap_plugin_latency { get: Some(lat_get) };

    unsafe extern "C" fn params_count(_p: *const clap_plugin) -> u32 {
        1
    }
    unsafe extern "C" fn params_get_info(
        _p: *const clap_plugin,
        idx: u32,
        info: *mut clap_param_info,
    ) -> bool {
        if idx != 0 || info.is_null() {
            return false;
        }
        let info = unsafe { &mut *info };
        info.id = 1;
        info.flags = CLAP_PARAM_IS_AUTOMATABLE;
        info.cookie = std::ptr::null_mut();
        info.name.fill(0);
        for (i, &b) in b"Gain".iter().enumerate() {
            info.name[i] = b as c_char;
        }
        info.module.fill(0);
        info.min_value = 0.0;
        info.max_value = 2.0;
        info.default_value = 0.5;
        true
    }
    unsafe extern "C" fn params_get_val(p: *const clap_plugin, id: u32, out: *mut f64) -> bool {
        if id != 1 || out.is_null() {
            return false;
        }
        let st = unsafe { &*((*p).plugin_data as *const StubPluginState) };
        unsafe { *out = st.gain };
        true
    }
    unsafe extern "C" fn params_val_text(
        _p: *const clap_plugin,
        _id: u32,
        _v: f64,
        _buf: *mut c_char,
        _len: u32,
    ) -> bool {
        false
    }
    static PARAMS_EXT: clap_plugin_params = clap_plugin_params {
        count: Some(params_count),
        get_info: Some(params_get_info),
        get_value: Some(params_get_val),
        value_to_text: Some(params_val_text),
        text_to_value: None,
        flush: None,
    };

    unsafe extern "C" fn state_save(p: *const clap_plugin, os: *const clap_ostream) -> bool {
        let st = unsafe { &*((*p).plugin_data as *const StubPluginState) };
        let bytes = st.gain.to_le_bytes();
        let write_fn = unsafe { (*os).write.unwrap() };
        let n = unsafe { write_fn(os, bytes.as_ptr() as *const c_void, 8) };
        n == 8
    }
    unsafe extern "C" fn state_load(p: *const clap_plugin, is: *const clap_istream) -> bool {
        let st = unsafe { &mut *((*p).plugin_data as *mut StubPluginState) };
        let mut bytes = [0u8; 8];
        let read_fn = unsafe { (*is).read.unwrap() };
        let n = unsafe { read_fn(is, bytes.as_mut_ptr() as *mut c_void, 8) };
        if n == 8 {
            st.gain = f64::from_le_bytes(bytes);
            true
        } else {
            false
        }
    }
    static STATE_EXT: clap_plugin_state = clap_plugin_state {
        save: Some(state_save),
        load: Some(state_load),
    };

    unsafe extern "C" fn plug_init(_p: *const clap_plugin) -> bool {
        true
    }
    unsafe extern "C" fn plug_destroy(p: *const clap_plugin) {
        if p.is_null() {
            return;
        }
        let b = unsafe { Box::from_raw(p as *mut clap_plugin) };
        if !b.plugin_data.is_null() {
            drop(unsafe { Box::from_raw(b.plugin_data as *mut StubPluginState) });
        }
    }
    unsafe extern "C" fn plug_activate(
        _p: *const clap_plugin,
        _sr: f64,
        _min: u32,
        _max: u32,
    ) -> bool {
        true
    }
    unsafe extern "C" fn plug_deactivate(_p: *const clap_plugin) {}
    unsafe extern "C" fn plug_start(_p: *const clap_plugin) -> bool {
        true
    }
    unsafe extern "C" fn plug_stop(_p: *const clap_plugin) {}
    unsafe extern "C" fn plug_reset(p: *const clap_plugin) {
        let st = unsafe { &mut *((*p).plugin_data as *mut StubPluginState) };
        st.gate = 0.0;
    }
    unsafe extern "C" fn plug_process(
        p: *const clap_plugin,
        proc: *const clap_process,
    ) -> clap_process_status {
        let st = unsafe { &mut *((*p).plugin_data as *mut StubPluginState) };
        let proc = unsafe { &*proc };
        if !proc.in_events.is_null() {
            let in_ev = unsafe { &*proc.in_events };
            if let (Some(sz), Some(get)) = (in_ev.size, in_ev.get) {
                let count = unsafe { sz(in_ev) };
                for idx in 0..count {
                    let h = unsafe { get(in_ev, idx) };
                    if !h.is_null() {
                        let ev = unsafe { &*h };
                        if ev.space_id == 0 {
                            match ev.type_ {
                                0 => st.gate = 1.0, // CLAP_EVENT_NOTE_ON
                                1 => st.gate = 0.0, // CLAP_EVENT_NOTE_OFF
                                5 => {
                                    // CLAP_EVENT_PARAM_VALUE
                                    let pv = unsafe {
                                        &*(h as *const clap_sys::events::clap_event_param_value)
                                    };
                                    if pv.param_id == 1 {
                                        st.gain = pv.value;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        if proc.audio_outputs_count > 0 && !proc.audio_outputs.is_null() {
            let out = unsafe { &*proc.audio_outputs };
            if out.channel_count >= 2 && !out.data32.is_null() {
                let ch0 = unsafe { *out.data32.add(0) };
                let ch1 = unsafe { *out.data32.add(1) };
                for f in 0..proc.frames_count as usize {
                    let in_v = if proc.audio_inputs_count > 0 && !proc.audio_inputs.is_null() {
                        let inp = unsafe { &*proc.audio_inputs };
                        if inp.channel_count >= 1 && !inp.data32.is_null() {
                            unsafe { *(*inp.data32).add(f) }
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    };
                    let val = if st.gate > 0.0 {
                        st.gain as f32
                    } else {
                        in_v * (st.gain as f32)
                    };
                    unsafe {
                        *ch0.add(f) = val;
                        *ch1.add(f) = val;
                    }
                }
            }
        }
        CLAP_PROCESS_CONTINUE
    }
    unsafe extern "C" fn plug_ext(_p: *const clap_plugin, id: *const c_char) -> *const c_void {
        if id.is_null() {
            return null();
        }
        let s = unsafe { CStr::from_ptr(id) };
        if s == CLAP_EXT_LATENCY {
            &LAT_EXT as *const _ as *const c_void
        } else if s == CLAP_EXT_PARAMS {
            &PARAMS_EXT as *const _ as *const c_void
        } else if s == CLAP_EXT_STATE {
            &STATE_EXT as *const _ as *const c_void
        } else {
            null()
        }
    }

    unsafe extern "C" fn fac_count(_f: *const clap_plugin_factory) -> u32 {
        1
    }
    unsafe extern "C" fn fac_desc(
        _f: *const clap_plugin_factory,
        i: u32,
    ) -> *const clap_plugin_descriptor {
        if i == 0 { &DESC } else { null() }
    }
    unsafe extern "C" fn fac_create(
        _f: *const clap_plugin_factory,
        _host: *const clap_host,
        pid: *const c_char,
    ) -> *const clap_plugin {
        if pid.is_null() || unsafe { CStr::from_ptr(pid) } != ID {
            return null();
        }
        let st = Box::into_raw(Box::new(StubPluginState {
            gain: 0.5,
            gate: 0.0,
        }));
        Box::into_raw(Box::new(clap_plugin {
            desc: &DESC,
            plugin_data: st as *mut c_void,
            init: Some(plug_init),
            destroy: Some(plug_destroy),
            activate: Some(plug_activate),
            deactivate: Some(plug_deactivate),
            start_processing: Some(plug_start),
            stop_processing: Some(plug_stop),
            reset: Some(plug_reset),
            process: Some(plug_process),
            get_extension: Some(plug_ext),
            on_main_thread: None,
        }))
    }
    static FACTORY: clap_plugin_factory = clap_plugin_factory {
        get_plugin_count: Some(fac_count),
        get_plugin_descriptor: Some(fac_desc),
        create_plugin: Some(fac_create),
    };

    unsafe extern "C" fn entry_init(_p: *const c_char) -> bool {
        true
    }
    unsafe extern "C" fn entry_deinit() {}
    unsafe extern "C" fn entry_fac(id: *const c_char) -> *const c_void {
        if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
            &FACTORY as *const _ as *const c_void
        } else {
            null()
        }
    }
    pub static STUB_ENTRY: EntryDescriptor = EntryDescriptor {
        clap_version: CLAP_VERSION,
        init: Some(entry_init),
        deinit: Some(entry_deinit),
        get_factory: Some(entry_fac),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrips() {
        for len in 0..20 {
            let src: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let enc = encode_b64(&src);
            let dec = decode_b64(&enc).unwrap();
            assert_eq!(dec, src);
        }
    }

    #[test]
    fn default_dirs_and_collect_handle_missing_roots() {
        let dirs = default_clap_dirs();
        assert!(!dirs.is_empty());
        let missing = vec![PathBuf::from("/nonexistent_thunder_clap_dir_12345")];
        assert!(collect_clap_files(&missing).is_empty());
    }

    #[test]
    fn isolated_scanner_handles_missing_file_gracefully() {
        let err = scan_clap_file_isolated(Path::new("/nonexistent_thunder_plugin.clap"));
        assert!(err.is_err());
    }

    #[test]
    fn stub_plugin_lifecycle_params_latency_state_and_audio() {
        let entry =
            unsafe { PluginEntry::load_from_raw(&test_stub::STUB_ENTRY, c"/tmp/stub.clap") }
                .unwrap();
        let descs = inspect_entry_plugins(&entry, "/tmp/stub.clap").unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].id, "org.cotanjant.thunder.test_stub");
        assert!(descs[0].is_instrument);

        let (mut main, mut node) = ClapMainInstance::instantiate_from_entry(
            entry,
            "org.cotanjant.thunder.test_stub",
            "",
            &[(1, 0.75)],
            44100.0,
        )
        .unwrap();
        assert_eq!(main.latency_samples, 32);
        assert_eq!(main.params.len(), 1);
        assert!((main.params[0].value - 0.75).abs() < 1e-6);

        let notes = [ClapBlockNote {
            sample_offset: 0,
            key: 60,
            vel: 0.9,
            pan: 0.0,
            fine_pitch: 0.0,
            dur_samples: 100,
        }];
        node.process_instrument_block(16, 120.0, true, 0, 0.0, &notes);
        assert!((node.out_l[0] - 0.75).abs() < 1e-5);
        assert!((node.out_r[15] - 0.75).abs() < 1e-5);

        let (dl, dr) = node.process_fx_frame(0.5, 0.5, 120.0, false);
        assert!(dl.is_finite() && dr.is_finite());

        let b64 = main.save_state_b64().unwrap();
        assert!(!b64.is_empty());
        main.load_state_b64(&b64).unwrap();
        drop(node);
    }
}
