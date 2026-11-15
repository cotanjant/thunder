# Plan: Getting Thunder to FL Studio Class

## 1. Current Baseline (Verified in Code)

Thunder (`v0.3.0` + unreleased `main`) is a working prototype across five source files (~4,550 lines):

- `src/song.rs`: `Song` (`SAVE_VERSION = 1`), `Channel`, `Pattern`, `Note`, `AutoPoint`, `AudioClip`, `SampleRef`. Fixed 8 default channels (`Kick`, `Snare`, `Clap`, `Hat`, `Bass`, `Lead`, `Pad`, `Sampler`), fixed `N_PATTERNS = 8`, fixed 1-bar (`64` tick) 4/4 patterns, single-lane `arrangement: Vec<i32>`, bar-aligned `audio_clips`.
- `src/engine.rs`: `cpal` output stream using `Arc<Mutex<AppState>>` and `try_lock()`, `fundsp` polyphonic synth pools (8 voices/channel), synthesized 4-piece kit, mono sample playback capped at 30s, single shared stereo delay and Schroeder reverb send, tanh master clip, and 5-minute in-memory master take recording.
- `src/ui.rs`: `egui` panels stacked inside one vertical `ScrollArea` (transport, browser, channel rack, piano roll with velocity + volume/cutoff automation lanes, playlist clip/audio lanes, mixer, tutorial). Supports undo/redo via full `Song` snapshots, copy/paste notes, duplicate pattern, basic note tools (quantize, humanize, strum, arpeggiate, Maj/Min chord stamp).
- `src/export.rs`: offline WAV mixdown (32-bit float stereo at 44.1 kHz) and SMF Type-0 MIDI export.
- `src/main.rs`: `eframe` host, FL dark color palette, Space/Ctrl+Z/Ctrl+Y/Ctrl+C/Ctrl+V shortcuts, `--offline-test` headless render gate.

### Structural Gaps Blocking FL-Class Work

1. **Audio thread safety**: `audio_callback` uses `Mutex::try_lock()` and outputs silence on contention. `EngineRt::process` sorts `pending`, mutates `Vec`s, and pushes into `take_l`/`take_r` on the audio thread.
2. **Incomplete channel signal path**: in `EngineRt::render_sample`, `Kit` and `Sampler` playheads bypass `channel.volume`, `channel.pan`, `channel.cutoff`, `auto_vol`, `mute`/`solo` post-trigger, and `ch_peaks`. Live preview uses a single monophonic Saw `live_voice` regardless of selected channel.
3. **Rigid song model**: fixed 8 channels, fixed 8 patterns of 1 bar, 4/4 only, no Mixer Inserts separate from Channel Rack instruments, single-row Playlist with bar-snapped audio clips, automation locked inside patterns rather than free Playlist clips.
4. **Editor ergonomics**: single-note selection only in Piano Roll, no zoom/pan, no slide/portamento notes, no multi-track Playlist editing, no native file dialogs, no drag-and-drop, all panels stacked in one scrolling page.
5. **Missing production essentials**: no insert FX chain (EQ, compressor, saturation, chorus), no sidechain routing, no hardware MIDI I/O, no mic/line-in recording, no third-party plugin hosting.

---

## 2. Target Definition

Reaching FL Studio class means a producer can compose, record, arrange, mix, and export a full multi-minute track without hitting artificial limits or audio dropouts:

- **Zero-glitch engine**: lock-free audio thread, sample-accurate timing, smooth parameter changes, Plugin Delay Compensation (PDC).
- **Signature FL workflow**: decoupled Channel Rack (generators), Piano Roll (deep MIDI + slide notes), Playlist (multi-track free-form Pattern, Audio, and Automation Clips), Mixer (routable inserts with 10 FX slots each), and Browser (tree + search + drag-and-drop).
- **Complete sound and I/O**: built-in synth/drum/sampler depth, essential mixing FX, hardware MIDI + MIDI learn, multi-track audio recording, stem/lossless export, and external plugin hosting.

---

## 3. Phase 1: Lock-Free Audio Core and Complete Signal Path

Goal: make the audio thread deterministic, glitch-free, and consistent across all channel types.

1. **Lock-free thread boundary**
   - Split `AppState` into UI/Project state (`Song` owned by UI thread) and `AudioEngine` (owned exclusively by the `cpal` callback).
   - Replace `Arc<Mutex<AppState>>` + `try_lock()` with an SPSC command ring buffer (`rtrb` or `ringbuf`) for UI -> Audio edits (note triggers, parameter changes, transport state, swapped pattern/channel snapshots) and an SPSC/atomic telemetry path for Audio -> UI (`playhead_tick`, `ch_peaks`, `master_peak_l/r`, `scope`, `xrun_count`).
   - Preallocate audio-thread buffers (`pending` event ring/fixed array, voice scratch buffers) so `process()` never allocates or locks.
   - Move take recording writes off the audio thread: stream recorded frames into a lock-free ring buffer drained by a dedicated background writer thread straight to a temporary WAV on disk (removes the 5-minute RAM cap).
2. **Fix channel routing parity**
   - Route `Kit` and `Sampler` playheads through per-channel gain, equal-power pan, filter (`cutoff`/`q`), ADSR envelope, `auto_vol`, `delay_send`, `reverb_send`, and `ch_peaks`.
   - Route `live_voice` preview through the currently selected channel's synth/sampler/kit configuration and polyphony.
3. **Parameter smoothing and transport clock**
   - Add per-sample one-pole smoothing on volume, pan, cutoff, and send levels to eliminate zipper noise during drags and automation.
   - Track exact fractional beat/tick timeline across BPM changes and loop boundaries; add audio-thread underrun (`xrun`) detection surfaced in the transport bar.
4. **Phase 1 verification**
   - Stress test: Hammer parameter updates and song swaps from a test thread while rendering audio blocks; assert zero allocations/locks in `process()`, finite output, and accurate `ch_peaks` for Synth, Kit, and Sampler channels.

---

## 4. Phase 2: Decoupled Song Model (Channels, Patterns, Playlist, Mixer Graph)

Goal: remove fixed counts and 1:1 coupling so songs scale to real projects.

1. **Dynamic Channel Rack**
   - Add, duplicate, delete, reorder, rename, and color channels in `Song::channels`.
   - Add `Channel::mixer_insert: usize` so any number of channels can route to any Mixer Insert (`Ctrl+L` links selected channel to selected insert).
   - Add Channel Settings panel: ADSR, filter, pitch/base note, polyphony cap / mono / portamento, cut/cut-by choke groups, mixer target.
2. **Flexible Patterns**
   - Dynamic `Song::patterns` list (add, clone, rename, color, delete, make unique) with variable bar length (1, 2, 4, 8+ bars) and project/pattern time signatures (numerator/denominator).
   - Keep step sequencer and piano roll synchronized on the same note data, plus a per-step graph editor in the rack for velocity, pan, and fine pitch.
3. **Multi-Track Playlist**
   - Replace `arrangement: Vec<i32>` and bar-locked `AudioClip` with `PlaylistTrack` lanes and free-positioned `Clip` structs (`start_tick: f64` or high-PPQ integer ticks, `len_ticks`, `offset_ticks`, `track: usize`, `mute: bool`, `color`).
   - Support three clip variants on any track:
     - `PatternClip { pattern_id }`
     - `AudioClip { sample_id, gain, pitch_semitones, stretch_mode, fade_in, fade_out }`
     - `AutomationClip { target_param, points, curves }`
   - Support optional FL "Track Mode" linking a Playlist track 1:1 to a Channel and Mixer Insert.
4. **Mixer Insert Routing Graph**
   - Separate `MixerTrack` list (Master + N Inserts, e.g. 64 or dynamic) from `Channel`.
   - Support arbitrary DAG routing between inserts (submix buses, parallel compression, dedicated FX sends, sidechain-only sends with 0% wet audio) with cycle detection and automatic Plugin Delay Compensation (PDC) across parallel paths.
5. **Versioned Migration**
   - Bump `SAVE_VERSION` with a backward-compatible loader (`v1 -> v2`) so existing `.json` projects open cleanly.

---

## 5. Phase 3: FL-Class Piano Roll

Goal: bring MIDI editing speed and expressiveness to FL Piano Roll standards.

1. **Selection, Tools, and Navigation**
   - Multi-note selection (`Ctrl+click`, Shift-box marquee, `Ctrl+A` all, `Ctrl+I` invert, `Ctrl+L` quick legato).
   - Multi-note transforms: drag move, resize, `Shift+drag` clone, `Ctrl+B` duplicate immediately after selection, `Alt+drag` snap bypass, batch velocity scaling (`Alt+wheel` or drag), batch transpose.
   - Tool palette: Draw, Paint (continuous brush placement), Slice/Knife (split notes along cut line), Mute, Erase, Playback/Scrub.
   - Independent horizontal and vertical zoom + scrollbar/middle-mouse pan; grid snap options (`Line`, `Cell`, `1/64`, `1/32`, `1/16`, `1/8`, `1/4`, `1/3` step/beat triplets, `None`).
2. **FL Signature Note Features**
   - **Slide notes and Portamento**: mark notes as slide targets (pitch/velocity/filter glide over the slide note duration) and per-channel mono portamento.
   - **Per-note property lanes**: switchable bottom lane for Velocity, Pan, Filter Cutoff, Fine Pitch, and Release.
   - **Ghost notes**: toggleable ghost notes from other channels in the same pattern, with optional "Editable ghosts" and double-click to jump to that channel.
3. **Composition Tools**
   - Expanded chord stamps (major, minor, dim, aug, sus2/4, 7ths, 9ths, inversions) and scale lock/highlighting (major, natural/harmonic/melodic minor, modes, pentatonic, blues).
   - Non-destructive modal/live previews for Quantize (with strength + swing), Strum (time + velocity curve), Flam, Riff/Arpeggiator patterns, Randomize, and Flip vertical/horizontal.

---

## 6. Phase 4: Instruments, Sampler Depth, and Mixer FX Rack

Goal: make stock sounds and mixing tools capable of finished production without external plugins.

1. **Upgraded Sampler and Drum Rack**
   - Preserve stereo samples, remove the 30s load cap (stream or load full stems with background peak-cache generation for waveform display).
   - Root-note pitch tracking across the keyboard (Hermite/cubic interpolation), ADSR + filter envelope, start/end trim, loop points with crossfade, reverse, normalize, DC offset removal.
   - Time-stretching / pitch-shifting (e.g. granular or phase-vocoder mode for tempo-synced vocal/drum loops).
   - Multi-pad Drum Rack channel (16-pad FPC-style kit with per-pad sample layers, velocity splits, choke groups, and individual mixer output offsets).
2. **Expanded Built-in Synths**
   - Multi-oscillator subtractive synth (3 oscillators, unison voices + detune + stereo spread, sub/noise, oscillator sync/ring mod, multimode resonant filter LP/BP/HP/Notch with drive and keytracking, 2 LFOs + mod matrix).
   - 4-operator FM synth for bells, metallic percussion, plucks, and basses.
   - Factory preset library + user preset save/load JSON.
3. **Per-Insert Effect Chain (10 Slots)**
   - Ordered `EffectSlot` chain on every Mixer Insert and Master, each with bypass toggle, wet/dry mix knob, reorder up/down, and stock DSP modules:
     - **Parametric EQ**: 7 bands (HP, LP, low/high shelf, bells) with real-time FFT spectrum analyzer.
     - **Compressor / Limiter**: threshold, ratio, knee, attack, release, makeup gain, external sidechain key input, gain-reduction meter, brickwall ceiling limiter on Master.
     - **Tempo-Synced Stereo Delay**: 1/4, 1/8, dotted, triplet sync, ping-pong mode, feedback lowpass/highpass damping.
     - **Algorithmic Reverb**: room size, decay, pre-delay, high-cut, stereo width.
     - **Modulation & Saturation**: Chorus/Flanger/Phaser, soft-clip/tube/wavefolder Saturator, Mid/Side Stereo Shaper.
   - Real dB-scaled faders (`-inf` to `+6 dB`), peak + RMS/LUFS metering, clip indicators, polarity invert, and stereo separation knob per insert.

---

## 7. Phase 5: Hardware MIDI, Audio Recording, and Automation Clips

Goal: support real hardware controllers, live tracking, and song-wide mix automation.

1. **Hardware MIDI (`midir`)**
   - Auto-discover and hot-refresh MIDI input/output devices in Settings.
   - Route MIDI input to the active or locked Channel Rack instrument; pass note-on/off, velocity, mod wheel, sustain pedal, and pitch bend into the audio thread via lock-free ring buffer.
   - MIDI recording into Piano Roll (count-in, overdub/replace, loop-merge, step-recording mode, optional auto-quantize).
   - **MIDI Learn (`Link to controller`)**: bind any hardware CC to any channel, synth, mixer, or FX parameter with mapping curve formulas (linear, log, inverted).
2. **Audio Input Recording**
   - Open `cpal` input stream on user-selected audio interface input channels; assign input pair/mono channel to any Mixer Insert.
   - Input monitoring toggle (with latency/PDC reporting), armed track state, count-in metronome, punch-in/out over loop region.
   - Stream recorded audio directly to disk into project takes folder and automatically place an `AudioClip` on the Playlist.
   - Basic sample editor modal (Edison-lite): zoomable waveform, trim, fade-in/out, normalize, slice at transients, drag selection onto Playlist or Sampler.
3. **Song-Wide Automation Clips**
   - Right-click any automatable control -> "Create automation clip" adds an `AutomationClip` spanning the selection on the Playlist.
   - Control points with adjustable curve tension (linear, smooth S-curve, hold/step, exponential) evaluated sample-accurately during playback and export.
   - Internal controller plugin (Peak Controller + LFO) so kick transients can drive bass volume or filter cutoff directly.

---

## 8. Phase 6: Workspace UX, Browser, Project Packaging, and Export

Goal: replace the prototype layout and textboxes with a fast desktop DAW interface.

1. **FL-Style Workspace and Windowing**
   - Replace the single vertical `ScrollArea` in `src/main.rs` with a dockable/floating panel workspace (`egui_dock` or multi-window viewport/panel manager) with resizable splitters, focus tracking, and F5 (Playlist), F6 (Channel Rack), F7 (Piano Roll), F8 (Browser), F9 (Mixer) hotkeys.
   - Add Typing-Keyboard-to-Piano mode (`Ctrl+T`) cleanly separated from command shortcuts.
   - Centralize shortcut dispatch with active-panel focus rules (`Ctrl+L` routes in Channel Rack/Mixer, performs Quick Legato in Piano Roll).
2. **Browser and Drag-and-Drop**
   - Tree view with pinned folders, favorites, instant text search, tags (Drums, Bass, FX, Presets), and synced-tempo loop preview.
   - Drag-and-drop samples/presets from Browser directly onto Channel Rack (create/replace channel), Playlist (create Audio Clip), or Mixer slots (load FX preset).
3. **Project Packaging and File UX**
   - Replace all file-path `TextEdit` fields with native OS file dialogs (`rfd`).
   - Add Autosave + crash recovery prompt on startup, Recent Projects menu, and New from Template.
   - Add Zipped Loop Package (`.zip`) export/import bundling `song.json` + all referenced audio samples so projects move cleanly across machines.
   - Add undo/redo commands covering structural edits (channels, mixer routing, playlist clips, FX slots) without duplicating sample PCM buffers (keep `Arc<[f32]>`).
4. **Pro Export Pipeline**
   - Format choices: WAV (16/24-bit PCM with TPDF dither, 32-bit float), FLAC, OGG, MP3.
   - Sample rate options (44.1 / 48 / 96 kHz), oversampled offline render option, tail handling (Cut remainder / Leave remainder / Wrap remainder), and **Split Mixer Tracks (Stem Export)**.
   - Standard MIDI File Type-1 import and export (multi-track, channel names, tempo map, time signature).

---

## 9. Phase 7: Third-Party Plugin Hosting (CLAP First, Optional VST3)

Note: `docs/ROADMAP.md` currently lists VST hosting as out of scope. Once Phases 1 through 5 are solid, third-party plugins become the main boundary between a self-contained DAW and an open studio platform.

1. **CLAP Host (`clack-host` + `clack-extensions`)**
   - Scan standard OS CLAP paths out-of-process (crash-safe scanner subprocess building a plugin database).
   - Host CLAP instruments inside Channel Rack slots and CLAP effects inside Mixer Insert FX slots.
   - Wire audio buffers, sample-accurate MIDI/note events, polyphonic modulation, parameter automation, state save/restore into `Song` JSON, and native plugin GUI embedding (`raw-window-handle` child window).
   - Read plugin latency (`clap_latency`) and feed it into Mixer PDC.
2. **VST3 Evaluation**
   - Evaluate VST3 hosting once CLAP hosting and PDC are proven, using an out-of-process or tightly sandboxed wrapper if cross-platform licensing and ABI stability warrant it.

---

## 10. Verification, Performance, and Release Gates

Maintain the existing four CI gates (`fmt`, `clippy -D warnings`, `cargo test`, `cargo run -- --offline-test`) and expand coverage with each phase:

1. **Audio Engine Tests**: lock-free command queue stress tests, zero-allocation assertions on `process()`, filter/EQ frequency response and compressor gain-reduction math tests, PDC alignment tests (impulse check across parallel routed inserts), sample-rate conversion accuracy tests.
2. **Data & Migration Tests**: `v1 -> v2` project upgrade fixtures, roundtrip tests for variable-length patterns, multi-track clips, automation curves, and mixer DAG routing (including cycle rejection).
3. **UI Interaction Tests (`egui_kittest`)**: multi-note marquee select/move/resize/delete, playlist clip paint/move/slice, mixer insert routing and FX slot bypass/reorder, undo/redo across structural edits.
4. **Hardware and Release Verification**:
   - Periodic latency, xrun, and GUI frame-time profiling on Linux (ALSA/PipeWire), Windows (WASAPI/ASIO), and macOS (CoreAudio).
   - Code signing (Apple Developer ID + notarization, Windows Authenticode) before shipping 1.0 installers, plus real-hardware click-through tests on MSI, DMG, and AppImage.
