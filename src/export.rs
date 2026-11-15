//! Song export: WAV mixdown (hound) and Standard MIDI File (hand-rolled).

use crate::engine::EngineRt;
use crate::song::{ClipKind, Song, TICKS_PATTERN};

/// Render the whole playlist (or one bar if empty) to stereo f32.
pub fn render_to_stereo(song: &Song, sample_rate: f32, once: bool) -> (Vec<f32>, Vec<f32>) {
    let mut rt = EngineRt::new(sample_rate);
    rt.ensure_channels(song);
    rt.song_mode = true;
    rt.ignore_loop = once;
    rt.playing = true;
    let spt = (sample_rate * 60.0 / song.bpm.max(30.0) / 16.0).max(1.0);
    let ticks = song
        .playlist_len_ticks()
        .max(song.time_sig.ticks_per_bar().max(TICKS_PATTERN));
    let total = (ticks as f32 * spt) as usize + (sample_rate * 2.0) as usize;
    let mut left = vec![0.0; total];
    let mut right = vec![0.0; total];
    let mut done = 0;
    while done < total && rt.playing {
        let n = 8192.min(total - done);
        rt.process(song, &mut left[done..done + n], &mut right[done..done + n]);
        done += n;
    }
    left.truncate(done);
    right.truncate(done);
    (left, right)
}

pub fn export_wav(song: &Song, path: &str) -> Result<String, String> {
    let sr = 44100.0;
    let (l, r) = render_to_stereo(song, sr, true);
    let n = write_wav_file(path, &l, &r, sr)?;
    Ok(format!("exported {n} samples to {path}"))
}

/// Shared WAV writer: mixdown export and recorded takes use the same path.
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

// ---------------------------------------------------------------------------
// Minimal SMF type-0 writer: tempo, time signature, note on/off.
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

pub fn export_midi(song: &Song, pattern_fallback: usize, path: &str) -> Result<String, String> {
    const PPQ: u32 = 480;
    const OUR_PER_QN: u32 = 16; // 64 ticks/bar / 4 beats
    let mut track: Vec<u8> = Vec::new();
    // Tempo.
    let mpqn = (60_000_000.0 / song.bpm.max(30.0)) as u32;
    track.extend([0x00, 0xff, 0x51, 0x03]);
    track.extend(mpqn.to_be_bytes()[1..4].iter());
    // Time signature from project.
    let ts_num = song.time_sig.num.clamp(1, 16);
    let ts_den_pow = match song.time_sig.den {
        2 => 1,
        8 => 3,
        16 => 4,
        _ => 2,
    };
    track.extend([0x00, 0xff, 0x58, 0x04, ts_num, ts_den_pow, 0x24, 0x08]);

    // Collect note events: (abs_our_tick, on/off, midi, vel).
    let mut evs: Vec<(u32, bool, u8, u8)> = Vec::new();
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
            for notes in &pdata.notes {
                for note in notes {
                    let abs_pat = cycle_base + note.tick;
                    if abs_pat < clip.offset_ticks || abs_pat >= clip.offset_ticks + clip.len_ticks
                    {
                        continue;
                    }
                    let rel = abs_pat - clip.offset_ticks;
                    let t = clip.start_tick + rel;
                    let len = note.len.max(1).min(clip.len_ticks - rel);
                    let v = (note.vel.clamp(0.0, 1.0) * 100.0) as u8 + 10;
                    evs.push((t, true, note.midi, v.min(127)));
                    evs.push((t + len, false, note.midi, 64));
                }
            }
        }
    }

    if !has_pattern_clips && !song.patterns.is_empty() {
        let pat = pattern_fallback.min(song.patterns.len() - 1);
        let pdata = &song.patterns[pat];
        max_tick = pdata.len_ticks();
        for notes in &pdata.notes {
            for note in notes {
                let v = (note.vel.clamp(0.0, 1.0) * 100.0) as u8 + 10;
                evs.push((note.tick, true, note.midi, v.min(127)));
                evs.push((note.tick + note.len.max(1), false, note.midi, 64));
            }
        }
    }

    let bar_ticks = song.time_sig.ticks_per_bar().max(1);
    let bars = max_tick.div_ceil(bar_ticks).max(1);

    evs.sort();
    let mut last = 0u32;
    for (t, on, midi, vel) in evs {
        vlq(t.saturating_sub(last) * (PPQ / OUR_PER_QN), &mut track);
        track.push(if on { 0x90 } else { 0x80 });
        track.push(midi);
        track.push(vel);
        last = t;
    }
    track.extend([0x00, 0xff, 0x2f, 0x00]);

    let mut file: Vec<u8> = Vec::new();
    file.extend(b"MThd");
    file.extend(6u32.to_be_bytes());
    file.extend(0u16.to_be_bytes());
    file.extend(1u16.to_be_bytes());
    file.extend((PPQ as u16).to_be_bytes());
    file.extend(b"MTrk");
    file.extend((track.len() as u32).to_be_bytes());
    file.extend(track);
    std::fs::write(path, file).map_err(|e| e.to_string())?;
    Ok(format!("exported {bars} bars to {path}"))
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

    #[test]
    fn midi_file_has_valid_header() {
        let path = tmp("thunder-midi-test.mid");
        let msg = export_midi(&demo_song(), 0, &path).unwrap();
        assert!(msg.contains("bars"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"MThd");
        assert_eq!(&bytes[8..10], [0x00, 0x00]); // format 0
        let mtrk = bytes
            .windows(4)
            .position(|w| w == b"MTrk")
            .expect("MTrk chunk");
        let track_len = u32::from_be_bytes(bytes[mtrk + 4..mtrk + 8].try_into().unwrap()) as usize;
        assert_eq!(mtrk + 8 + track_len, bytes.len());
        assert!(bytes.contains(&0x90)); // at least one note-on
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn midi_falls_back_to_single_pattern() {
        let mut song = crate::song::new_empty();
        song.patterns[0].notes[0].push(crate::song::Note::new(0, 4, 60, 0.9));
        let path = tmp("thunder-midi-fallback.mid");
        let msg = export_midi(&song, 0, &path).unwrap();
        assert!(msg.starts_with("exported 1 bars"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"MThd");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn midi_exports_multi_track_variable_pattern() {
        let mut song = crate::song::new_empty();
        song.patterns[0].bars = 2;
        song.patterns[0].notes[0].push(crate::song::Note::new(80, 4, 60, 0.9));
        song.add_pattern_clip(1, 0, 0);
        let path = tmp("thunder-midi-multitrack.mid");
        let msg = export_midi(&song, 0, &path).unwrap();
        assert!(msg.starts_with("exported 2 bars"));
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.contains(&0x90));
        std::fs::remove_file(&path).ok();
    }
}
