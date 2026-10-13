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

Ordered by what users feel first.

1. Undo/redo. The biggest missing editor feature. Nothing else matters
   as much for daily use.
2. Copy/paste notes and duplicate patterns. Editing one note at a time
   gets old fast.
3. Audio clips on the playlist. Recorded takes and imported files should
   sit on the timeline, not just play from the browser.
4. Input recording. The take recorder captures the master bus only.
   Mic and line-in with monitoring comes next.
5. MIDI hardware in and out, plus MIDI learn for knobs.
6. More sound: sampler envelopes and choke groups, EQ, compressor,
   chorus. The mixer already has sends, it needs per-track slots.
7. Keyboard shortcuts beyond Space. Arrows, Ctrl+Z, Delete already work
   in places, the rest is unmapped.
8. Signed builds. The Gatekeeper and SmartScreen warnings stay until
   Apple and Microsoft certificates are paid for. Nothing code-side
   fixes that.
9. In-app update check pointing at the latest release.
10. Example projects in the repo and a short video on the site, so new
    users see the workflow before installing.

Out of scope: mobile apps, VST hosting, live collaboration.
