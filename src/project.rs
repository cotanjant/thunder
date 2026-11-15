//! Project management: templates, recent projects, browser prefs, autosave/crash recovery,
//! and Zipped Loop Package (.zip) export/import.

use crate::engine::load_sample_file;
use crate::song::{
    Channel, ChannelKind, EffectSlot, FmSynthCfg, FxKind, KitPiece, Note, ReverbFxCfg, Song,
    SuperSynthCfg, compute_sample_peaks, new_empty, palette_color,
};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TemplateKind {
    Empty,
    Trap808,
    House128,
    Lofi85,
}

impl TemplateKind {
    pub const ALL: [TemplateKind; 4] = [
        TemplateKind::Empty,
        TemplateKind::Trap808,
        TemplateKind::House128,
        TemplateKind::Lofi85,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            TemplateKind::Empty => "Empty 4/4 (128 BPM)",
            TemplateKind::Trap808 => "808 Trap (140 BPM)",
            TemplateKind::House128 => "House (128 BPM)",
            TemplateKind::Lofi85 => "Lo-Fi Beats (85 BPM)",
        }
    }
}

fn add_notes(song: &mut Song, ch: usize, steps: &[u32], midi: u8, len: u32, vel: f32) {
    if let Some(pat) = song.patterns.get_mut(0)
        && let Some(list) = pat.notes.get_mut(ch)
    {
        for &s in steps {
            list.push(Note::new(s * 4, len, midi, vel));
        }
    }
}

pub fn template_song(kind: TemplateKind) -> Song {
    let mut s = new_empty();
    match kind {
        TemplateKind::Empty => s,
        TemplateKind::Trap808 => {
            s.bpm = 140.0;
            add_notes(&mut s, 0, &[0, 6, 10], 60, 4, 0.95);
            add_notes(&mut s, 2, &[4, 12], 60, 4, 0.9);
            add_notes(
                &mut s,
                3,
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
                60,
                2,
                0.7,
            );
            add_notes(&mut s, 4, &[0, 6, 10], 36, 12, 0.95);
            let bell = Channel {
                name: "Trap Bell".into(),
                kind: ChannelKind::FmSynth(FmSynthCfg::default()),
                color: palette_color(5),
                cutoff: 8000.0,
                reverb_send: 0.35,
                mixer_insert: 6,
                ..Default::default()
            };
            let ch = s.add_channel(bell);
            add_notes(&mut s, ch, &[0, 4, 8, 12], 72, 6, 0.8);
            s.add_pattern_clip(0, 0, 0);
            s.add_pattern_clip(0, 0, 64);
            s
        }
        TemplateKind::House128 => {
            s.bpm = 128.0;
            add_notes(&mut s, 0, &[0, 4, 8, 12], 60, 4, 0.95);
            add_notes(&mut s, 2, &[4, 12], 60, 4, 0.85);
            add_notes(&mut s, 3, &[2, 6, 10, 14], 60, 3, 0.8);
            add_notes(&mut s, 4, &[2, 6, 10, 14], 41, 6, 0.88);
            let sup = Channel {
                name: "House Stab".into(),
                kind: ChannelKind::SuperSynth(SuperSynthCfg::default()),
                color: palette_color(6),
                cutoff: 3200.0,
                reverb_send: 0.3,
                delay_send: 0.25,
                mixer_insert: 5,
                ..Default::default()
            };
            let ch = s.add_channel(sup);
            for m in [57, 60, 64] {
                add_notes(&mut s, ch, &[0, 6, 12], m, 8, 0.72);
            }
            if let Some(m) = s.mixer_tracks.get_mut(5) {
                m.add_fx(EffectSlot::new(FxKind::Reverb(ReverbFxCfg::default())));
            }
            s.add_pattern_clip(0, 0, 0);
            s.add_pattern_clip(0, 0, 64);
            s
        }
        TemplateKind::Lofi85 => {
            s.bpm = 85.0;
            s.swing = 0.35;
            add_notes(&mut s, 0, &[0, 7, 10], 60, 4, 0.85);
            add_notes(&mut s, 1, &[4, 12], 60, 4, 0.78);
            add_notes(&mut s, 3, &[0, 2, 4, 6, 8, 10, 12, 14], 60, 3, 0.55);
            let keys = Channel {
                name: "Dust Keys".into(),
                kind: ChannelKind::Kit {
                    piece: KitPiece::Kick,
                },
                ..Default::default()
            };
            let _ = keys;
            s.channels[5].cutoff = 1400.0;
            s.channels[5].reverb_send = 0.45;
            for m in [53, 57, 60, 64] {
                add_notes(&mut s, 5, &[0, 8], m, 24, 0.65);
            }
            s.add_pattern_clip(0, 0, 0);
            s
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("thunder"))
}

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|p| p.join("thunder"))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RecentProjects {
    pub paths: Vec<String>,
}

impl RecentProjects {
    pub fn load_from(dir: &Path) -> Self {
        let p = dir.join("recent.json");
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save_to(&self, dir: &Path) {
        if std::fs::create_dir_all(dir).is_ok()
            && let Ok(json) = serde_json::to_string_pretty(self)
        {
            std::fs::write(dir.join("recent.json"), json).ok();
        }
    }

    pub fn push(&mut self, path: &str) {
        let p = path.trim().to_string();
        if p.is_empty() {
            return;
        }
        self.paths.retain(|x| x != &p);
        self.paths.insert(0, p);
        self.paths.truncate(10);
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BrowserPrefs {
    pub pinned_dirs: Vec<String>,
    pub favorites: Vec<String>,
}

impl BrowserPrefs {
    pub fn load_from(dir: &Path) -> Self {
        let p = dir.join("browser.json");
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save_to(&self, dir: &Path) {
        if std::fs::create_dir_all(dir).is_ok()
            && let Ok(json) = serde_json::to_string_pretty(self)
        {
            std::fs::write(dir.join("browser.json"), json).ok();
        }
    }
}

pub fn reload_song_samples(song: &mut Song) -> usize {
    let mut missing = 0usize;
    for smp in song.samples.iter_mut() {
        if !smp.path.is_empty() {
            match load_sample_file(&smp.path) {
                Ok((data, data_r, sr)) => {
                    smp.data = data.into();
                    smp.data_r = data_r.into();
                    smp.sr = sr;
                    smp.peaks = compute_sample_peaks(&smp.data, &smp.data_r);
                }
                Err(_) => missing += 1,
            }
        }
    }
    missing
}

pub fn load_song_from_path(path: &Path) -> Result<(Song, usize), String> {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
    {
        let extract = data_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("zip_work");
        let song = import_zip_package(path, &extract)?;
        return Ok((song, 0));
    }
    let s_path = path.to_string_lossy();
    let mut song = Song::load(&s_path)?;
    let missing = reload_song_samples(&mut song);
    Ok((song, missing))
}

// ---------------------------------------------------------------------------
// Autosave + Crash Recovery
// ---------------------------------------------------------------------------

pub fn has_crash_recovery(dir: &Path) -> bool {
    dir.join("session.lock").exists() && dir.join("autosave.json").exists()
}

pub fn mark_session_active(dir: &Path) {
    if std::fs::create_dir_all(dir).is_ok() {
        std::fs::write(dir.join("session.lock"), "active").ok();
    }
}

pub fn clear_session_active(dir: &Path) {
    std::fs::remove_file(dir.join("session.lock")).ok();
}

pub fn discard_recovery(dir: &Path) {
    std::fs::remove_file(dir.join("session.lock")).ok();
    std::fs::remove_file(dir.join("autosave.json")).ok();
}

pub fn write_autosave(dir: &Path, song: &Song) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string(song).map_err(|e| e.to_string())?;
    let tmp = dir.join("autosave.json.tmp");
    let dst = dir.join("autosave.json");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &dst).map_err(|e| e.to_string())?;
    mark_session_active(dir);
    Ok(())
}

pub fn load_autosave(dir: &Path) -> Result<(Song, usize), String> {
    let p = dir.join("autosave.json");
    let mut song = Song::load(&p.to_string_lossy())?;
    let missing = reload_song_samples(&mut song);
    Ok((song, missing))
}

// ---------------------------------------------------------------------------
// Zipped Loop Package (.zip)
// ---------------------------------------------------------------------------

fn encode_sample_wav_bytes(l: &[f32], r: &[f32], sr: f32) -> Result<Vec<u8>, String> {
    let stereo = !r.is_empty();
    let spec = hound::WavSpec {
        channels: if stereo { 2 } else { 1 },
        sample_rate: sr.max(8000.0) as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut cur = Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cur, spec).map_err(|e| e.to_string())?;
        if stereo {
            for (i, &lv) in l.iter().enumerate() {
                let rv = r.get(i).copied().unwrap_or(lv);
                w.write_sample(lv).map_err(|e| e.to_string())?;
                w.write_sample(rv).map_err(|e| e.to_string())?;
            }
        } else {
            for &lv in l {
                w.write_sample(lv).map_err(|e| e.to_string())?;
            }
        }
        w.finalize().map_err(|e| e.to_string())?;
    }
    Ok(cur.into_inner())
}

fn sanitize_zip_name(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("sample");
    stem.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn export_zip_package(song: &Song, zip_path: &Path) -> Result<String, String> {
    let file = std::fs::File::create(zip_path).map_err(|e| e.to_string())?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut packaged = song.clone();
    let mut sample_count = 0usize;
    for (idx, smp) in packaged.samples.iter_mut().enumerate() {
        let base = sanitize_zip_name(&smp.name);
        let rel = format!("samples/{idx:02}_{base}.wav");
        let wav_bytes = encode_sample_wav_bytes(&smp.data, &smp.data_r, smp.sr)?;
        zw.start_file(&rel, opts).map_err(|e| e.to_string())?;
        zw.write_all(&wav_bytes).map_err(|e| e.to_string())?;
        smp.path = rel;
        sample_count += 1;
    }

    let json = serde_json::to_string_pretty(&packaged).map_err(|e| e.to_string())?;
    zw.start_file("song.json", opts)
        .map_err(|e| e.to_string())?;
    zw.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
    zw.finish().map_err(|e| e.to_string())?;

    Ok(format!(
        "exported zipped package ({sample_count} samples) to {}",
        zip_path.display()
    ))
}

pub fn import_zip_package(zip_path: &Path, extract_dir: &Path) -> Result<Song, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut za = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(extract_dir).map_err(|e| e.to_string())?;

    let mut song_json: Option<String> = None;
    for i in 0..za.len() {
        let mut entry = za.by_index(i).map_err(|e| e.to_string())?;
        let Some(enclosed) = entry.enclosed_name() else {
            continue;
        };
        if enclosed == Path::new("song.json") {
            let mut s = String::new();
            entry.read_to_string(&mut s).map_err(|e| e.to_string())?;
            song_json = Some(s);
        } else if !entry.is_dir() {
            let out_path = extract_dir.join(&enclosed);
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        }
    }

    let raw = song_json.ok_or("zip package does not contain song.json")?;
    let mut song = Song::from_json_str(&raw)?;
    for smp in &mut song.samples {
        if !smp.path.is_empty() {
            let full = extract_dir.join(&smp.path);
            smp.path = full.to_string_lossy().into_owned();
        }
    }
    let _ = reload_song_samples(&mut song);
    Ok(song)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::song::SampleRef;

    #[test]
    fn templates_produce_valid_songs() {
        for kind in TemplateKind::ALL {
            let s = template_song(kind);
            assert!(s.bpm >= 60.0);
            assert!(!s.channels.is_empty());
            assert!(!s.patterns.is_empty());
        }
        let trap = template_song(TemplateKind::Trap808);
        assert_eq!(trap.bpm, 140.0);
        assert!(!trap.clips.is_empty());
    }

    #[test]
    fn autosave_crash_recovery_lifecycle() {
        let dir = std::env::temp_dir().join("thunder-autosave-test");
        std::fs::remove_dir_all(&dir).ok();
        assert!(!has_crash_recovery(&dir));

        let mut song = template_song(TemplateKind::House128);
        song.bpm = 132.0;
        write_autosave(&dir, &song).unwrap();
        assert!(has_crash_recovery(&dir));

        let (restored, missing) = load_autosave(&dir).unwrap();
        assert_eq!(missing, 0);
        assert_eq!(restored.bpm, 132.0);

        clear_session_active(&dir);
        assert!(!has_crash_recovery(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zipped_loop_package_roundtrips_samples() {
        let dir = std::env::temp_dir().join("thunder-zip-pkg-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let mut song = template_song(TemplateKind::Empty);
        let smp = SampleRef::from_stereo(
            "pad_loop.wav",
            "in_memory",
            vec![0.1, 0.25, -0.3, 0.4],
            vec![-0.1, 0.2, 0.3, -0.4],
            44100.0,
        );
        song.samples.push(smp);

        let zip_path = dir.join("project_bundle.zip");
        export_zip_package(&song, &zip_path).unwrap();
        assert!(zip_path.exists());

        let extract_dir = dir.join("unpacked");
        let loaded = import_zip_package(&zip_path, &extract_dir).unwrap();
        assert_eq!(loaded.samples.len(), 1);
        assert_eq!(loaded.samples[0].data.len(), 4);
        assert!((loaded.samples[0].data[1] - 0.25).abs() < 1e-5);
        assert!((loaded.samples[0].data_r[3] - (-0.4)).abs() < 1e-5);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recent_projects_dedupes_and_caps() {
        let mut rec = RecentProjects::default();
        for i in 0..15 {
            rec.push(&format!("/tmp/song_{i}.json"));
        }
        assert_eq!(rec.paths.len(), 10);
        assert_eq!(rec.paths[0], "/tmp/song_14.json");
        rec.push("/tmp/song_10.json");
        assert_eq!(rec.paths[0], "/tmp/song_10.json");
        assert_eq!(rec.paths.len(), 10);
    }
}
