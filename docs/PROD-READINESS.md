# Prod-readiness audit — thunder-daw

Status: v0.2.5 (cpal 0.18, eframe/egui 0.36, fundsp 0.23, edition 2024).

## Done

- Dependencies are current, no pinned old majors in `[dependencies]`
- CI (`.github/workflows/ci.yml`): fmt, clippy `-D warnings`, `cargo check`,
  `cargo test`, `--offline-test`, on Linux, Windows and macOS
- 13 unit tests: automation interp, quantize, strum, chord, humanize,
  step roundtrip, save/load roundtrip, arpeggiator, MIDI file header and
  fallback, engine scheduler fires, NaN-free render
- Dependabot: cargo + github-actions, weekly, all merged
- Releases through v0.2.5 on GitHub: MSI, DMG, AppImage, shell/PowerShell
  installers, Homebrew formula, old-glibc Linux builds, CHANGELOG.md
- Docs site + landing page (`website/`), deployed to GitHub Pages (`pages.yml`)
- README with requirements, run and test instructions
- MIT license
- Self-test (`--offline-test`) renders headless and asserts non-silence
- `.gitignore` covers `/target`; `Cargo.lock` is committed

## Not done

- Audio thread uses `Mutex::try_lock` and drops to silence on contention.
  It holds up in practice; it is not built for stage use.
- Fixed 8 channels. No add/remove, no VST hosting, no audio recording,
  no MIDI hardware I/O.
- MSI, DMG and AppImage contents are verified structurally. None of them
  has been clicked through on real hardware.
