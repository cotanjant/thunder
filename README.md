# thunder

A desktop FL Studio-style DAW in Rust: channel rack, piano roll, multi-track playlist, and routable mixer.

- Audio through [cpal](https://crates.io/crates/cpal) with a lock-free worker and disk take streaming
- Dockable interface with [egui](https://crates.io/crates/egui) and [egui_dock](https://crates.io/crates/egui_dock) via [eframe](https://crates.io/crates/eframe)
- Built-in DSP from [fundsp](https://crates.io/crates/fundsp), custom synth/FX engines, and third-party CLAP plugins via [clack-host](https://crates.io/crates/clack-host)

## Install

Prebuilt binaries for Windows, macOS and Linux are attached to every
[release](https://github.com/cotanjant/thunder/releases).
Pick the one for your system on the
[download page](https://cotanjant.github.io/thunder/releases.html):
Windows gets an MSI wizard, Mac a drag-to-Applications disk image,
Linux an AppImage. No terminal required.

## Features

- Channel Rack: dynamic channels with add, duplicate, reorder, mixer routing, choke groups, and step sequencer
- Instruments: SuperSynth (3 oscillators, unison, multimode filter, LFO matrix), 4-operator FM synth, 16-pad Drum Rack, synthesized drum kit, and stereo sampler with looping, reverse, and granular time-stretch
- CLAP plugins: isolated out-of-process scanner, instrument and FX slot hosting, parameter UI, native plugin GUIs, state saving in project files, and latency compensation
- Piano Roll: multi-note selection, marquee/slice/paint tools, slide and portamento notes, ghost notes, scale lock, chord stamps, property lanes, quantize, humanize, strum, and arpeggiate
- Playlist: multi-track arrangement with Pattern, Audio, and Automation clips, optional Track Mode, punch-in/out loop region, and curve tension automation
- Mixer: routable inserts with cycle detection, automatic Plugin Delay Compensation (PDC), sidechain sends, 10 FX slots per insert (7-band EQ with FFT, compressor/limiter, tempo delay, reverb, mod FX, saturator, M/S shaper), Peak Controller + LFO, dB faders, and LUFS/RMS metering
- Recording and MIDI: hardware MIDI input/output (`midir`), CC MIDI learn, typing-keyboard piano, live audio input monitoring and disk recording, plus a built-in sample editor (trim, fade, normalize, transient slicing)
- Workspace and export: dockable panels (`F5`-`F9`), drag-and-drop browser, autosave crash recovery, zipped loop packages (`.zip`), WAV (16/24-bit dithered, 32-bit float), FLAC, OGG, MP3, stem split export, and Type-1 MIDI import/export

## Build from source

You need a Rust toolchain (1.88 or newer):

```bash
git clone https://github.com/cotanjant/thunder.git
cd thunder
cargo run --release
```

Linux also needs the audio and windowing headers:

```bash
sudo apt install libasound2-dev libx11-dev libxkbcommon-dev libwayland-dev libegl-dev libgl-dev
```

Windows and macOS need nothing extra.

## Tests

Unit tests plus a headless audio render that fails on silence or NaN:

```bash
cargo test
cargo run -- --offline-test
```

## Website and docs

The site in [`website/`](website/) deploys to GitHub Pages on every push to `main`.
Releases live under [GitHub Releases](https://github.com/cotanjant/thunder/releases)
with the [changelog](CHANGELOG.md) in the repo.
