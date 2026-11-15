// GUI app: no console window on Windows double-click. Terminal users still
// see stdout/stderr in their own console; only release builds detach.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod clap_host;
mod engine;
mod export;
mod fx;
mod midi;
mod project;
mod song;
mod synth;
mod ui;

use engine::{AppState, start_audio};
use raw_window_handle::HasWindowHandle;
use ui::AppUi;

struct DawApp {
    _stream: Option<cpal::Stream>,
    state: AppState,
    ui: AppUi,
}

/// FL Studio look: near-black blue-gray, green LEDs and selections.
fn fl_visuals() -> eframe::egui::Visuals {
    let mut v = eframe::egui::Visuals::dark();
    let bg = eframe::egui::Color32::from_rgb(24, 27, 33);
    let panel = eframe::egui::Color32::from_rgb(18, 20, 25);
    v.override_text_color = Some(eframe::egui::Color32::from_rgb(225, 228, 232));
    v.window_fill = panel;
    v.panel_fill = panel;
    v.faint_bg_color = bg;
    v.extreme_bg_color = eframe::egui::Color32::from_rgb(12, 13, 16);
    v.selection.bg_fill = eframe::egui::Color32::from_rgb(46, 120, 52);
    v.widgets.noninteractive.bg_fill = bg;
    v.widgets.inactive.bg_fill = eframe::egui::Color32::from_rgb(44, 49, 58);
    v.widgets.hovered.bg_fill = eframe::egui::Color32::from_rgb(58, 64, 76);
    v.widgets.active.bg_fill = eframe::egui::Color32::from_rgb(52, 110, 58);
    v
}

impl eframe::App for DawApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
        self.ui.parent_window = frame.window_handle().ok().map(|h| h.as_raw());
        self.state.sync_audio();
        ui::sync_clap_plugins(&mut self.ui, &mut self.state);
        let target_ch = self
            .ui
            .lock_midi_ch
            .unwrap_or(self.ui.sel_ch)
            .min(self.state.song.channels.len().saturating_sub(1));
        if self.state.rt.midi_target_ch != target_ch {
            self.state.set_midi_target(target_ch);
        }
        let snap_ticks = self.ui.rec_snap_ticks();
        while let Ok(msg) = self.ui.midi.ui_rx.pop() {
            if self.state.audio.is_none() {
                self.state.send_midi_msg(msg);
            }
            if let midi::MidiMsg::Cc { ch, cc, val } = msg {
                self.ui
                    .midi_learn
                    .process_cc(&mut self.state.song, ch, cc, val);
            }
            self.ui.midi_rec.handle_msg(
                &mut self.state.song,
                msg,
                midi::MidiRecCtx {
                    pat: self.ui.sel_pat,
                    ch: target_ch,
                    cur_tick: self.state.rt.cur_tick,
                    playing: self.state.rt.playing,
                    snap_grid: snap_ticks,
                },
            );
        }
        if !self.state.rt.playing
            && self.ui.midi_rec.mode != midi::MidiRecMode::StepInput
            && (!self.ui.midi_rec.held.is_empty() || self.ui.midi_rec.replaced_pass)
        {
            self.ui.midi_rec.reset_pass();
        }
        let pointer_down = ui.ctx().input(|i| i.pointer.any_down());
        self.ui.record_song(&self.state.song, pointer_down);
        ui::process_typing_piano(&mut self.ui, &mut self.state, ui.ctx());
        ui::handle_workspace_shortcuts(&mut self.ui, &mut self.state, ui.ctx());
        ui::sync_dock_tabs(&mut self.ui);
        eframe::egui::Panel::top("transport").show(ui, |u| {
            ui::transport(&mut self.ui, &mut self.state, u);
        });
        if self.ui.show_browser {
            eframe::egui::Panel::left("browser")
                .default_size(240.0)
                .show(ui, |u| {
                    eframe::egui::ScrollArea::vertical().show(u, |u| {
                        ui::browser(&mut self.ui, &mut self.state, u);
                    });
                });
        }
        eframe::egui::Panel::bottom("hint").show(ui, |u| {
            u.horizontal(|u| {
                if self.ui.typing_piano {
                    u.strong(format!("[Keys C{}]", self.ui.typing_octave));
                    u.separator();
                }
                u.label(format!("status: {}", self.ui.status));
            });
        });
        eframe::egui::CentralPanel::default().show(ui, |u| {
            ui::tutorial(&mut self.ui, &mut self.state, u);
            ui::show_dock_area(&mut self.ui, &mut self.state, u);
        });
        ui::edison_window(&mut self.ui, &mut self.state, ui.ctx());
        ui::export_window(&mut self.ui, &mut self.state, ui.ctx());
        ui::recovery_window(&mut self.ui, &mut self.state, ui.ctx());
        let pointer_down = ui.ctx().input(|i| i.pointer.any_down());
        self.ui.record_song(&self.state.song, pointer_down);
        self.ui.maybe_autosave(&self.state.song);
        self.state.sync_audio();
    }

    fn on_exit(&mut self) {
        if let Some(d) = project::data_dir() {
            project::clear_session_active(&d);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--scan-clap"
            && let Some(path) = args.next()
        {
            std::process::exit(clap_host::run_scan_cli(&path));
        }
    }
    if std::env::args().any(|a| a == "--offline-test") {
        let mut st = AppState::new();
        // The default project is blank; the self-test needs sound to find.
        st.song = song::demo_song();
        let sr = st.rt.sr;
        st.rt.set_sample_rate(sr, &st.song);
        st.rt.song_mode = true;
        st.rt.playing = true;
        let n = 44100 * 4;
        let mut l = vec![0.0; n];
        let mut r = vec![0.0; n];
        st.rt.process(&st.song.clone(), &mut l, &mut r);
        let peak = l
            .iter()
            .chain(r.iter())
            .map(|x| x.abs())
            .fold(0.0f32, f32::max);
        let rms = (l.iter().map(|x| x * x).sum::<f32>() / l.len() as f32).sqrt();
        println!(
            "offline-test: peak={peak:.3} rms={rms:.4} tick_pos={:.1}",
            st.rt.tick_pos
        );
        if peak < 0.01 || st.rt.tick_pos < 10.0 {
            eprintln!("offline-test FAILED");
            std::process::exit(1);
        }
        println!("offline-test OK");
        return;
    }
    let mut state = AppState::new();
    let stream = match start_audio(&mut state) {
        Some(st) => {
            println!("audio started");
            Some(st)
        }
        None => {
            eprintln!("no audio device; running GUI silent.");
            None
        }
    };
    let mut app_ui = AppUi::default();
    app_ui.midi.refresh();
    app_ui.audio_in_ports = midi::list_audio_inputs();
    if let Some(tx) = state.audio.as_mut().and_then(|b| b.take_midi_rt_tx()) {
        app_ui.midi.set_audio_tx(tx);
    }
    if let Some(d) = project::data_dir() {
        if project::has_crash_recovery(&d) {
            app_ui.show_recovery_prompt = true;
        } else {
            project::mark_session_active(&d);
        }
    }
    let options = eframe::NativeOptions::default();
    if let Err(e) = eframe::run_native(
        "Thunder",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(fl_visuals());
            Ok(Box::new(DawApp {
                state,
                ui: app_ui,
                _stream: stream,
            }) as Box<dyn eframe::App>)
        }),
    ) {
        eprintln!("eframe error: {e}");
    }
}
