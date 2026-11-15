//! Song export & MIDI I/O: WAV (16-bit TPDF / 24-bit / 32f), FLAC, OGG Vorbis,
//! MP3, oversampled rendering, tail modes, split mixer stems, and SMF Type-1 import/export.

use crate::engine::EngineRt;
use crate::song::{Channel, ClipKind, Note, Song, TICKS_PATTERN, TimeSig, palette_color};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ExportFormat {
    #[default]
    Wav32f,
    Wav24,
    Wav16,
    Flac16,
    Flac24,
    Mp3,
    Ogg,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 7] = [
        ExportFormat::Wav16,
        ExportFormat::Wav24,
        ExportFormat::Wav32f,
        ExportFormat::Flac16,
        ExportFormat::Flac24,
        ExportFormat::Mp3,
        ExportFormat::Ogg,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            ExportFormat::Wav16 => "WAV 16-bit (TPDF)",
            ExportFormat::Wav24 => "WAV 24-bit",
            ExportFormat::Wav32f => "WAV 32-bit float",
            ExportFormat::Flac16 => "FLAC 16-bit",
            ExportFormat::Flac24 => "FLAC 24-bit",
            ExportFormat::Mp3 => "MP3 192k",
            ExportFormat::Ogg => "OGG Vorbis",
        }
    }

    pub fn ext(&self) -> &'static str {
        match self {
            ExportFormat::Wav16 | ExportFormat::Wav24 | ExportFormat::Wav32f => "wav",
            ExportFormat::Flac16 | ExportFormat::Flac24 => "flac",
            ExportFormat::Mp3 => "mp3",
            ExportFormat::Ogg => "ogg",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TailMode {
    Cut,
    #[default]
    Leave,
    Wrap,
}

impl TailMode {
    pub const ALL: [TailMode; 3] = [TailMode::Cut, TailMode::Leave, TailMode::Wrap];

    pub fn name(&self) -> &'static str {
        match self {
            TailMode::Cut => "Cut remainder",
            TailMode::Leave => "Leave remainder",
            TailMode::Wrap => "Wrap remainder",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ExportConfig {
    pub format: ExportFormat,
    pub sample_rate: u32,
    pub oversample: u8,
    pub tail_mode: TailMode,
    pub split_mixer_tracks: bool,
    pub mp3_kbps: u32,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            format: ExportFormat::Wav32f,
            sample_rate: 44100,
            oversample: 1,
            tail_mode: TailMode::Leave,
            split_mixer_tracks: false,
            mp3_kbps: 192,
        }
    }
}

fn downsample_by(buf: &[f32], factor: usize) -> Vec<f32> {
    if factor <= 1 {
        return buf.to_vec();
    }
    let out_len = buf.len() / factor;
    let inv = 1.0 / factor as f32;
    let mut out = Vec::with_capacity(out_len);
    for chunk in buf.chunks_exact(factor) {
        out.push(chunk.iter().sum::<f32>() * inv);
    }
    out
}

/// Render with sample rate, oversampling (`1x`, `2x`, `4x`), and tail mode.
pub fn render_to_stereo_cfg(song: &Song, cfg: &ExportConfig) -> (Vec<f32>, Vec<f32>) {
    let os = (cfg.oversample.clamp(1, 4)) as usize;
    let out_sr = cfg.sample_rate.max(8000) as f32;
    let render_sr = out_sr * os as f32;
    let spt_out = (out_sr * 60.0 / song.bpm.max(30.0) / 16.0).max(1.0);
    let song_ticks = song
        .playlist_len_ticks()
        .max(song.time_sig.ticks_per_bar().max(TICKS_PATTERN));
    let song_frames_out = (song_ticks as f32 * spt_out).round().max(1.0) as usize;
    let tail_secs = match cfg.tail_mode {
        TailMode::Cut => 0.0,
        TailMode::Leave => 4.0,
        TailMode::Wrap => 2.0,
    };
    let tail_frames_out = (out_sr * tail_secs) as usize;
    let total_render = (song_frames_out + tail_frames_out) * os;

    let mut rt = EngineRt::new(render_sr);
    rt.ensure_channels(song);
    let (_clap_mains, clap_nodes) = crate::clap_host::mount_all_for_offline(song, render_sr);
    rt.clap_nodes = clap_nodes;
    rt.song_mode = true;
    rt.ignore_loop = true;
    rt.playing = true;

    let mut raw_l = vec![0.0f32; total_render];
    let mut raw_r = vec![0.0f32; total_render];
    let mut done = 0usize;
    while done < total_render {
        let n = 8192.min(total_render - done);
        if cfg.tail_mode != TailMode::Cut && done >= song_frames_out * os {
            // Stop scheduling new notes at song end while letting FX/voice tails ring.
            rt.playing = false;
        }
        rt.process(song, &mut raw_l[done..done + n], &mut raw_r[done..done + n]);
        done += n;
    }
    drop(rt);
    drop(_clap_mains);

    let mut l = downsample_by(&raw_l, os);
    let mut r = downsample_by(&raw_r, os);
    match cfg.tail_mode {
        TailMode::Cut => {
            l.truncate(song_frames_out);
            r.truncate(song_frames_out);
        }
        TailMode::Leave => {
            let mut last_loud = song_frames_out.min(l.len());
            for i in song_frames_out..l.len() {
                if l[i].abs().max(r[i].abs()) > 1e-4 {
                    last_loud = i + 1;
                }
            }
            let keep = (last_loud + 256).min(l.len());
            l.truncate(keep);
            r.truncate(keep);
        }
        TailMode::Wrap => {
            let base = song_frames_out.min(l.len()).max(1);
            for i in base..l.len() {
                let dst = i % base;
                l[dst] = (l[dst] + l[i]).clamp(-1.0, 1.0);
                r[dst] = (r[dst] + r[i]).clamp(-1.0, 1.0);
            }
            l.truncate(base);
            r.truncate(base);
        }
    }
    (l, r)
}

pub fn export_audio(song: &Song, path: &str, cfg: &ExportConfig) -> Result<String, String> {
    if cfg.split_mixer_tracks {
        return export_stems(song, path, cfg);
    }
    let (l, r) = render_to_stereo_cfg(song, cfg);
    let n = write_encoded_audio(path, &l, &r, cfg)?;
    Ok(format!(
        "exported {n} frames ({}) to {path}",
        cfg.format.name()
    ))
}

fn export_stems(song: &Song, base_path: &str, cfg: &ExportConfig) -> Result<String, String> {
    let p = Path::new(base_path);
    let dir = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("thunder");
    let ext = cfg.format.ext();

    // Master mixdown + each active non-master insert that carries channels, clips, or FX.
    let mut active_inserts = vec![0usize];
    for i in 1..song.mixer_tracks.len() {
        let used_ch = song.channels.iter().any(|c| c.mixer_insert == i);
        let used_fx = !song.mixer_tracks[i].fx.is_empty();
        let used_tr = song
            .playlist_tracks
            .iter()
            .any(|t| t.linked_insert == Some(i));
        if used_ch || used_fx || used_tr {
            active_inserts.push(i);
        }
    }

    let mut written = 0usize;
    for ins in active_inserts {
        let mut s_copy = song.clone();
        for mt in &mut s_copy.mixer_tracks {
            mt.solo = false;
        }
        if ins > 0 {
            s_copy.mixer_tracks[ins].solo = true;
        }
        let track_name = s_copy
            .mixer_tracks
            .get(ins)
            .map(|m| m.name.replace([' ', '/', '\\', ':'], "_"))
            .unwrap_or_else(|| format!("Insert_{ins}"));
        let out_path = dir.join(format!("{stem}_{track_name}.{ext}"));
        let out_str = out_path.to_string_lossy();
        let (l, r) = render_to_stereo_cfg(&s_copy, cfg);
        write_encoded_audio(&out_str, &l, &r, cfg)?;
        written += 1;
    }
    Ok(format!(
        "exported {written} stem files to {}",
        dir.display()
    ))
}

fn write_encoded_audio(
    path: &str,
    l: &[f32],
    r: &[f32],
    cfg: &ExportConfig,
) -> Result<usize, String> {
    let sr = cfg.sample_rate.max(8000) as f32;
    match cfg.format {
        ExportFormat::Wav32f => write_wav_file(path, l, r, sr),
        ExportFormat::Wav24 => write_wav_24(path, l, r, sr as u32),
        ExportFormat::Wav16 => write_wav_16_tpdf(path, l, r, sr as u32),
        ExportFormat::Flac16 => write_flac(path, l, r, sr as u32, 16),
        ExportFormat::Flac24 => write_flac(path, l, r, sr as u32, 24),
        ExportFormat::Mp3 => write_mp3(path, l, r, sr as u32, cfg.mp3_kbps),
        ExportFormat::Ogg => write_ogg(path, l, r, sr as u32),
    }
}

/// Shared WAV writer: 32-bit float stereo mixdown and recorded takes use the same path.
pub fn write_wav_file(path: &str, l: &[f32], r: &[f32], sr: f32) -> Result<usize, String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sr as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for (a, b) in l.iter().zip(r.iter()) {
        w.write_sample(*a).map_err(|e| e.to_string())?;
        w.write_sample(*b).map_err(|e| e.to_string())?;
    }
    w.finalize().map_err(|e| e.to_string())?;
    Ok(l.len().min(r.len()))
}

fn lcg_unit(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*state >> 40) as f32) / ((1u64 << 24) as f32)
}

fn write_wav_16_tpdf(path: &str, l: &[f32], r: &[f32], sr: u32) -> Result<usize, String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sr,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    for (a, b) in l.iter().zip(r.iter()) {
        for &s in &[a, b] {
            let dither = (lcg_unit(&mut rng) - lcg_unit(&mut rng)) / 32768.0;
            let q = ((s.clamp(-1.0, 1.0) + dither) * 32767.0).round() as i32;
            w.write_sample(q.clamp(-32768, 32767) as i16)
                .map_err(|e| e.to_string())?;
        }
    }
    w.finalize().map_err(|e| e.to_string())?;
    Ok(l.len().min(r.len()))
}

fn write_wav_24(path: &str, l: &[f32], r: &[f32], sr: u32) -> Result<usize, String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sr,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    let scale = 8_388_607.0f32;
    for (&a, &b) in l.iter().zip(r.iter()) {
        w.write_sample((a.clamp(-1.0, 1.0) * scale).round() as i32)
            .map_err(|e| e.to_string())?;
        w.write_sample((b.clamp(-1.0, 1.0) * scale).round() as i32)
            .map_err(|e| e.to_string())?;
    }
    w.finalize().map_err(|e| e.to_string())?;
    Ok(l.len().min(r.len()))
}

fn write_flac(path: &str, l: &[f32], r: &[f32], sr: u32, bits: usize) -> Result<usize, String> {
    use flacenc::bitsink::ByteSink;
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let n = l.len().min(r.len());
    let scale = if bits == 24 { 8_388_607.0 } else { 32_767.0 };
    let lo = if bits == 24 { -8_388_608 } else { -32_768 };
    let hi = if bits == 24 { 8_388_607 } else { 32_767 };
    let mut interleaved = Vec::with_capacity(n * 2);
    for i in 0..n {
        interleaved.push(((l[i].clamp(-1.0, 1.0) * scale).round() as i32).clamp(lo, hi));
        interleaved.push(((r[i].clamp(-1.0, 1.0) * scale).round() as i32).clamp(lo, hi));
    }
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| format!("flac config error: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(&interleaved, 2, bits, sr as usize);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| format!("flac encode failed: {e:?}"))?;
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| format!("flac write failed: {e:?}"))?;
    std::fs::write(path, sink.as_slice()).map_err(|e| e.to_string())?;
    Ok(n)
}

fn write_mp3(path: &str, l: &[f32], r: &[f32], sr: u32, kbps: u32) -> Result<usize, String> {
    let n = l.len().min(r.len());
    let mut interleaved = Vec::with_capacity(n * 2);
    for i in 0..n {
        interleaved.push(l[i].clamp(-1.0, 1.0));
        interleaved.push(r[i].clamp(-1.0, 1.0));
    }
    // MPEG Layer III accepts up to 48 kHz; halve 96 kHz automatically if requested.
    let (pcm, enc_sr) = if sr > 48000 {
        let mut down = Vec::with_capacity((n / 2) * 2);
        for pair in interleaved.as_chunks::<4>().0 {
            down.push(0.5 * (pair[0] + pair[2]));
            down.push(0.5 * (pair[1] + pair[3]));
        }
        (down, sr / 2)
    } else {
        (interleaved, sr)
    };
    let mut enc = rusty_mp3::Mp3Encoder::new(rusty_mp3::Mp3EncoderConfig {
        bitrate_kbps: kbps,
        vbr_quality: None,
    });
    enc.push_pcm_f32(&pcm, 2, enc_sr)
        .map_err(|e| format!("mp3 encode failed: {e:?}"))?;
    enc.finish();
    let mut out = Vec::new();
    while let Ok(pkt) = enc.next_packet() {
        out.extend_from_slice(&pkt);
    }
    std::fs::write(path, out).map_err(|e| e.to_string())?;
    Ok(n)
}

fn write_ogg(path: &str, l: &[f32], r: &[f32], sr: u32) -> Result<usize, String> {
    let n = l.len().min(r.len());
    let pcm = vec![l[..n].to_vec(), r[..n].to_vec()];
    let mut cfg = oxideav_vorbis::oggfile::StreamEncoderConfig::new(sr, 2);
    cfg.quality = 0.7;
    let bytes =
        oxideav_vorbis::encode_pcm_to_ogg(&pcm, &cfg).map_err(|e| format!("ogg encode: {e}"))?;
    std::fs::write(path, bytes).map_err(|e| e.to_string())?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Standard MIDI File (SMF Type-1 multi-track export + Type-0/1 import)
// ---------------------------------------------------------------------------

fn vlq(mut v: u32, out: &mut Vec<u8>) {
    let mut buf = [0u8; 5];
    let mut n = 1;
    buf[4] = (v & 0x7f) as u8;
    v >>= 7;
    while v > 0 {
        n += 1;
        buf[5 - n] = ((v & 0x7f) as u8) | 0x80;
        v >>= 7;
    }
    out.extend_from_slice(&buf[5 - n..]);
}

fn push_meta_text(track: &mut Vec<u8>, meta_ty: u8, text: &str) {
    let b = text.as_bytes();
    track.push(0x00);
    track.push(0xff);
    track.push(meta_ty);
    vlq(b.len() as u32, track);
    track.extend_from_slice(b);
}

fn push_track_chunk(file: &mut Vec<u8>, data: &[u8]) {
    file.extend(b"MTrk");
    file.extend((data.len() as u32).to_be_bytes());
    file.extend(data);
}

pub fn export_midi(song: &Song, pattern_fallback: usize, path: &str) -> Result<String, String> {
    const PPQ: u32 = 480;
    const OUR_PER_QN: u32 = 16; // 64 ticks/bar / 4 beats
    let nch = song.channels.len().max(1);
    let mut per_ch: Vec<Vec<(u32, bool, u8, u8)>> = vec![Vec::new(); nch];
    let mut has_pattern_clips = false;
    let mut max_tick = 0u32;

    for clip in &song.clips {
        if clip.mute {
            continue;
        }
        let ClipKind::Pattern { pattern } = clip.kind else {
            continue;
        };
        let Some(pdata) = song.patterns.get(pattern) else {
            continue;
        };
        has_pattern_clips = true;
        max_tick = max_tick.max(clip.start_tick.saturating_add(clip.len_ticks));
        let pat_len = pdata.len_ticks().max(1);
        let first_cycle = clip.offset_ticks / pat_len;
        let last_cycle = (clip.offset_ticks + clip.len_ticks.saturating_sub(1)) / pat_len;
        for cycle in first_cycle..=last_cycle {
            let cycle_base = cycle * pat_len;
            for (ci, notes) in pdata.notes.iter().enumerate() {
                if ci >= per_ch.len() {
                    continue;
                }
                for note in notes {
                    if note.mute || note.slide {
                        continue;
                    }
                    let abs_pat = cycle_base + note.tick;
                    if abs_pat < clip.offset_ticks || abs_pat >= clip.offset_ticks + clip.len_ticks
                    {
                        continue;
                    }
                    let rel = abs_pat - clip.offset_ticks;
                    let t = clip.start_tick + rel;
                    let len = note.len.max(1).min(clip.len_ticks - rel);
                    let v = (note.vel.clamp(0.0, 1.0) * 100.0) as u8 + 10;
                    per_ch[ci].push((t, true, note.midi, v.min(127)));
                    per_ch[ci].push((t + len, false, note.midi, 64));
                }
            }
        }
    }

    if !has_pattern_clips && !song.patterns.is_empty() {
        let pat = pattern_fallback.min(song.patterns.len() - 1);
        let pdata = &song.patterns[pat];
        max_tick = pdata.len_ticks();
        for (ci, notes) in pdata.notes.iter().enumerate() {
            if ci >= per_ch.len() {
                continue;
            }
            for note in notes {
                if note.mute || note.slide {
                    continue;
                }
                let v = (note.vel.clamp(0.0, 1.0) * 100.0) as u8 + 10;
                per_ch[ci].push((note.tick, true, note.midi, v.min(127)));
                per_ch[ci].push((note.tick + note.len.max(1), false, note.midi, 64));
            }
        }
    }

    let bar_ticks = song.time_sig.ticks_per_bar().max(1);
    let bars = max_tick.div_ceil(bar_ticks).max(1);

    // Track 0: tempo + time signature + project name.
    let mut meta_trk = Vec::new();
    push_meta_text(&mut meta_trk, 0x03, "Thunder Tempo");
    let mpqn = (60_000_000.0 / song.bpm.max(30.0)) as u32;
    meta_trk.extend([0x00, 0xff, 0x51, 0x03]);
    meta_trk.extend(mpqn.to_be_bytes()[1..4].iter());
    let ts_num = song.time_sig.num.clamp(1, 16);
    let ts_den_pow = match song.time_sig.den {
        2 => 1,
        8 => 3,
        16 => 4,
        _ => 2,
    };
    meta_trk.extend([0x00, 0xff, 0x58, 0x04, ts_num, ts_den_pow, 0x24, 0x08]);
    meta_trk.extend([0x00, 0xff, 0x2f, 0x00]);

    let mut tracks: Vec<Vec<u8>> = vec![meta_trk];
    for (ci, mut evs) in per_ch.into_iter().enumerate() {
        if evs.is_empty() && ci > 0 {
            continue;
        }
        let mut trk = Vec::new();
        let cname = song
            .channels
            .get(ci)
            .map(|c| c.name.as_str())
            .unwrap_or("Channel");
        push_meta_text(&mut trk, 0x03, cname);
        let midi_ch = (ci as u8) & 0x0f;
        evs.sort();
        let mut last = 0u32;
        for (t, on, midi, vel) in evs {
            vlq(t.saturating_sub(last) * (PPQ / OUR_PER_QN), &mut trk);
            trk.push((if on { 0x90 } else { 0x80 }) | midi_ch);
            trk.push(midi);
            trk.push(vel);
            last = t;
        }
        trk.extend([0x00, 0xff, 0x2f, 0x00]);
        tracks.push(trk);
    }

    let mut file: Vec<u8> = Vec::new();
    file.extend(b"MThd");
    file.extend(6u32.to_be_bytes());
    file.extend(1u16.to_be_bytes()); // SMF Type-1
    file.extend((tracks.len() as u16).to_be_bytes());
    file.extend((PPQ as u16).to_be_bytes());
    for t in &tracks {
        push_track_chunk(&mut file, t);
    }
    std::fs::write(path, file).map_err(|e| e.to_string())?;
    Ok(format!(
        "exported {bars} bars ({} tracks) to {path}",
        tracks.len()
    ))
}

#[derive(Debug, Clone)]
pub struct ImportedMidiTrack {
    pub name: String,
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone)]
pub struct ImportedMidi {
    pub bpm: Option<f32>,
    pub time_sig: Option<TimeSig>,
    pub tracks: Vec<ImportedMidiTrack>,
}

fn read_vlq(data: &[u8], pos: &mut usize) -> Result<u32, String> {
    let mut val = 0u32;
    for _ in 0..4 {
        let b = *data.get(*pos).ok_or("unexpected EOF in MIDI VLQ")?;
        *pos += 1;
        val = (val << 7) | u32::from(b & 0x7f);
        if b & 0x80 == 0 {
            return Ok(val);
        }
    }
    Err("malformed MIDI VLQ".into())
}

pub fn import_midi_file(path: &str) -> Result<ImportedMidi, String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() < 14 || &data[0..4] != b"MThd" {
        return Err("not a valid SMF MIDI file".into());
    }
    let hdr_len = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
    if hdr_len < 6 || 8 + hdr_len > data.len() {
        return Err("truncated MIDI header".into());
    }
    let ntrks = u16::from_be_bytes(data[10..12].try_into().unwrap()) as usize;
    let division = u16::from_be_bytes(data[12..14].try_into().unwrap());
    let ppq = if division & 0x8000 == 0 {
        u32::from(division).max(1)
    } else {
        480
    };

    let mut pos = 8 + hdr_len;
    let mut bpm = None;
    let mut time_sig = None;
    let mut tracks = Vec::new();

    for ti in 0..ntrks {
        if pos + 8 > data.len() {
            break;
        }
        if &data[pos..pos + 4] != b"MTrk" {
            return Err("expected MTrk chunk".into());
        }
        let len = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        let end = (pos + len).min(data.len());
        let trk = &data[pos..end];
        pos = end;

        let mut tp = 0usize;
        let mut abs_midi = 0u64;
        let mut running = 0u8;
        let mut trk_name = format!("MIDI Track {}", ti + 1);
        let mut open_notes: Vec<(u8, u64, f32)> = Vec::new();
        let mut notes: Vec<Note> = Vec::new();

        while tp < trk.len() {
            let delta = u64::from(read_vlq(trk, &mut tp)?);
            abs_midi += delta;
            let Some(&b0) = trk.get(tp) else { break };
            let status = if b0 & 0x80 != 0 {
                tp += 1;
                if b0 < 0xf0 {
                    running = b0;
                }
                b0
            } else if running != 0 {
                running
            } else {
                return Err("missing MIDI running status".into());
            };

            if status == 0xff {
                let meta_ty = *trk.get(tp).ok_or("EOF in meta")?;
                tp += 1;
                let mlen = read_vlq(trk, &mut tp)? as usize;
                let mdata = trk.get(tp..tp + mlen).ok_or("EOF in meta data")?;
                tp += mlen;
                match meta_ty {
                    0x03 if !mdata.is_empty() => {
                        if let Ok(s) = std::str::from_utf8(mdata) {
                            trk_name = s.trim().to_string();
                        }
                    }
                    0x51 if mdata.len() == 3 => {
                        let mpqn = (u32::from(mdata[0]) << 16)
                            | (u32::from(mdata[1]) << 8)
                            | u32::from(mdata[2]);
                        if mpqn > 0 {
                            bpm = Some((60_000_000.0 / mpqn as f32).clamp(40.0, 240.0));
                        }
                    }
                    0x58 if mdata.len() >= 2 => {
                        let num = mdata[0].clamp(1, 16);
                        let den = 1u8
                            .checked_shl(u32::from(mdata[1]))
                            .unwrap_or(4)
                            .clamp(2, 16);
                        time_sig = Some(TimeSig { num, den });
                    }
                    0x2f => break,
                    _ => {}
                }
            } else if status == 0xf0 || status == 0xf7 {
                let slen = read_vlq(trk, &mut tp)? as usize;
                tp = (tp + slen).min(trk.len());
            } else {
                let kind = status & 0xf0;
                let dlen = match kind {
                    0xc0 | 0xd0 => 1,
                    _ => 2,
                };
                if tp + dlen > trk.len() {
                    break;
                }
                let d1 = trk[tp];
                let d2 = if dlen == 2 { trk[tp + 1] } else { 0 };
                tp += dlen;
                let to_our_tick = |mt: u64| ((mt * 16) / u64::from(ppq)) as u32;
                match kind {
                    0x90 if d2 > 0 => {
                        open_notes.push((d1, abs_midi, (f32::from(d2) / 127.0).clamp(0.05, 1.0)));
                    }
                    0x80 | 0x90 => {
                        if let Some(oi) = open_notes.iter().rposition(|&(m, _, _)| m == d1) {
                            let (_, start_m, vel) = open_notes.remove(oi);
                            let t0 = to_our_tick(start_m);
                            let t1 = to_our_tick(abs_midi).max(t0 + 1);
                            notes.push(Note::new(t0, t1 - t0, d1, vel));
                        }
                    }
                    _ => {}
                }
            }
        }
        if !notes.is_empty() {
            notes.sort_by_key(|n| (n.tick, n.midi));
            tracks.push(ImportedMidiTrack {
                name: trk_name,
                notes,
            });
        }
    }

    Ok(ImportedMidi {
        bpm,
        time_sig,
        tracks,
    })
}

pub fn import_midi_into_song(song: &mut Song, pat: usize, path: &str) -> Result<usize, String> {
    let parsed = import_midi_file(path)?;
    if parsed.tracks.is_empty() {
        return Err("MIDI file contains no notes".into());
    }
    if let Some(b) = parsed.bpm {
        song.bpm = b;
    }
    if let Some(ts) = parsed.time_sig {
        song.time_sig = ts;
    }
    let pat = pat.min(song.patterns.len().saturating_sub(1));
    let mut max_tick = 0u32;
    let mut imported_notes = 0usize;
    for (idx, trk) in parsed.tracks.iter().enumerate() {
        let ch = if idx < song.channels.len() {
            idx
        } else {
            let c = Channel {
                name: trk.name.clone(),
                color: palette_color(idx),
                mixer_insert: (idx + 1).min(song.mixer_tracks.len().saturating_sub(1)),
                ..Default::default()
            };
            song.add_channel(c)
        };
        for n in &trk.notes {
            max_tick = max_tick.max(n.tick + n.len);
        }
        imported_notes += trk.notes.len();
        song.patterns[pat].notes[ch] = trk.notes.clone();
    }
    let bar_ticks = song.time_sig.ticks_per_bar().max(1);
    let bars_needed = max_tick.div_ceil(bar_ticks).clamp(1, 32) as u16;
    if bars_needed > song.patterns[pat].bars {
        song.patterns[pat].bars = bars_needed;
    }
    Ok(imported_notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::song::demo_song;

    fn tmp(name: &str) -> String {
        std::env::temp_dir()
            .join(name)
            .to_str()
            .unwrap()
            .to_string()
    }

    fn short_song() -> Song {
        let mut s = crate::song::new_empty();
        s.bpm = 150.0;
        s.patterns[0].notes[4].push(Note::new(0, 8, 60, 0.9));
        s.patterns[0].notes[4].push(Note::new(16, 8, 64, 0.85));
        s.add_pattern_clip(0, 0, 0);
        s
    }

    #[test]
    fn midi_file_has_valid_type1_header_and_roundtrips() {
        let path = tmp("thunder-midi-type1.mid");
        let msg = export_midi(&demo_song(), 0, &path).unwrap();
        assert!(msg.contains("bars"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"MThd");
        assert_eq!(&bytes[8..10], [0x00, 0x01]); // format 1

        let imported = import_midi_file(&path).unwrap();
        assert!(!imported.tracks.is_empty());
        assert!((imported.bpm.unwrap() - demo_song().bpm).abs() < 1.0);
        assert!(imported.tracks.iter().map(|t| t.notes.len()).sum::<usize>() >= 8);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn midi_import_populates_song_pattern() {
        let path = tmp("thunder-midi-into-song.mid");
        export_midi(&demo_song(), 0, &path).unwrap();
        let mut s = crate::song::new_empty();
        let count = import_midi_into_song(&mut s, 0, &path).unwrap();
        assert!(count >= 8);
        assert!(!s.patterns[0].notes.iter().all(|v| v.is_empty()));
        std::fs::remove_file(&path).ok();
    }

    fn check_export_format(fmt: ExportFormat, name: &str) {
        let song = short_song();
        let p = tmp(name);
        let cfg = ExportConfig {
            format: fmt,
            sample_rate: 44100,
            oversample: 1,
            tail_mode: TailMode::Cut,
            split_mixer_tracks: false,
            mp3_kbps: 192,
        };
        export_audio(&song, &p, &cfg).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        assert!(meta.len() > 256, "{name} too small: {}", meta.len());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn export_wav16_and_wav24_work() {
        check_export_format(ExportFormat::Wav16, "thunder-exp-16.wav");
        check_export_format(ExportFormat::Wav24, "thunder-exp-24.wav");
    }

    #[test]
    fn export_flac_works() {
        check_export_format(ExportFormat::Flac16, "thunder-exp.flac");
    }

    #[test]
    fn export_mp3_works() {
        check_export_format(ExportFormat::Mp3, "thunder-exp.mp3");
    }

    #[test]
    fn export_ogg_works() {
        check_export_format(ExportFormat::Ogg, "thunder-exp.ogg");
    }

    #[test]
    fn oversample_and_tail_wrap_render_finite_audio() {
        let song = short_song();
        let (l, r) = render_to_stereo_cfg(
            &song,
            &ExportConfig {
                format: ExportFormat::Wav32f,
                sample_rate: 16000,
                oversample: 2,
                tail_mode: TailMode::Wrap,
                split_mixer_tracks: false,
                mp3_kbps: 192,
            },
        );
        assert!(!l.is_empty() && l.len() == r.len());
        assert!(l.iter().all(|x| x.is_finite()));
        assert!(l.iter().map(|x| x.abs()).fold(0.0f32, f32::max) > 0.01);
    }

    #[test]
    fn split_mixer_tracks_exports_multiple_stems() {
        let dir = std::env::temp_dir().join("thunder-stems-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("track.wav");
        let mut s = short_song();
        for ch in &mut s.channels {
            ch.mixer_insert = 1;
        }
        let cfg = ExportConfig {
            format: ExportFormat::Wav16,
            sample_rate: 22050,
            tail_mode: TailMode::Cut,
            split_mixer_tracks: true,
            ..Default::default()
        };
        let msg = export_audio(&s, base.to_str().unwrap(), &cfg).unwrap();
        assert!(msg.contains("stem"));
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
