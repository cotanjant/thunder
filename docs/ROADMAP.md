# Thunder: rebrand notes and roadmap

## What was done (rebrand session)

The product is now Thunder, not thunder-daw.

- Repo moved `canureal/thunder-daw` to `cotanjant/thunder`. The account
  had been renamed too, so the remote, the tap reference and every doc
  link moved with it.
- Binary and package renamed to `thunder`. Window title, tutorial text,
  default project name and config folder followed.
- macOS app is Thunder.app with bundle id `com.cotanjant.thunder`.
  DMG, AppImage, tarballs, MSI, installer scripts and the Homebrew
  formula (`thunder.rb`, published at 0.3.0) all use the new names.
- WiX: product and manufacturer renamed, shortcuts and support link kept,
  auto-EULA untouched.
- Old release files keep their old names. CHANGELOG history was not
  rewritten. The Download page links each version to its real filenames.
- v0.3.0 tagged and shipped: Release, package and compat workflows all
  green. MSI contents, AppImage run, DMG structure and brew formula
  version verified by hand.

## How to reach FL level

Ordered by what users feel first. Items 1 through 7 are now implemented on `main` (see [`PLAN.md`](../PLAN.md) and [`CHANGELOG.md`](../CHANGELOG.md)):

1. Done: Undo/redo across pattern edits and structural changes (shared `Arc<[f32]>` sample buffers).
2. Done: Copy/paste notes, multi-note marquee/transforms, duplicate/unique patterns, and Piano Roll slide notes.
3. Done: Multi-track Playlist with Pattern, Audio, and Automation clips plus drag-and-drop from Browser.
4. Done: Disk-streamed master/input recording, input monitoring, punch-in/out, and built-in sample editor.
5. Done: Hardware MIDI in/out (`midir`), recording modes, and CC MIDI learn.
6. Done: Stereo granular sampler, 16-pad Drum Rack, SuperSynth, 4-op FM synth, and 10-slot Mixer FX rack with DAG routing and automatic PDC.
7. Done: Keyboard shortcuts with panel-focus dispatch, typing piano (`Ctrl+T`), and dockable panels (`F5`-`F9`).
8. Open: Signed builds. Gatekeeper and SmartScreen warnings stay until Apple and Microsoft certificates are paid for. Nothing code-side fixes that.
9. Open: In-app update check pointing at the latest release.
10. Open: Example projects in the repo and a short video on the site, so new users see the workflow before installing.

Out of scope: mobile apps, live collaboration. Native CLAP plugin hosting is implemented; VST3 hosting remains deferred.
