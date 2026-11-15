// GUI app: no console window on Windows double-click. Terminal users still
// see stdout/stderr in their own console; only release builds detach.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod engine;
mod export;
mod fx;
mod midi;
mod song;
mod synth;
mod ui;

use engine::{AppState, start_audio};
use ui::AppUi;

struct DawApp {
    state: AppState,
    ui: AppUi,
    _stream: Option<cpal::Stream>,
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
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
        self.state.sync_audio();
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
        // Space toggles transport, like FL. Not while typing in a text box.
        if !ui.ctx().egui_wants_keyboard_input()
            && ui.ctx().input(|i| i.key_pressed(eframe::egui::Key::Space))
        {
            let next = !self.state.rt.playing;
            self.state.set_playing(next);
        }
        if !ui.ctx().egui_wants_keyboard_input() {
            let (command, shift, z, y, c, v, l, a, i_key, b, f5, f6, f7, f8, f9) =
                ui.ctx().input(|i| {
                    (
                        i.modifiers.command,
                        i.modifiers.shift,
                        i.key_pressed(eframe::egui::Key::Z),
                        i.key_pressed(eframe::egui::Key::Y),
                        i.key_pressed(eframe::egui::Key::C),
                        i.key_pressed(eframe::egui::Key::V),
                        i.key_pressed(eframe::egui::Key::L),
                        i.key_pressed(eframe::egui::Key::A),
                        i.key_pressed(eframe::egui::Key::I),
                        i.key_pressed(eframe::egui::Key::B),
                        i.key_pressed(eframe::egui::Key::F5),
                        i.key_pressed(eframe::egui::Key::F6),
                        i.key_pressed(eframe::egui::Key::F7),
                        i.key_pressed(eframe::egui::Key::F8),
                        i.key_pressed(eframe::egui::Key::F9),
                    )
                });
            if f5 {
                self.ui.show_playlist = !self.ui.show_playlist;
            }
            if f6 {
                self.ui.show_rack = !self.ui.show_rack;
            }
            if f7 {
                self.ui.show_piano = !self.ui.show_piano;
            }
            if f8 {
                self.ui.show_browser = !self.ui.show_browser;
            }
            if f9 {
                self.ui.show_mixer = !self.ui.show_mixer;
            }
            if command && z && shift {
                ui::redo(&mut self.ui, &mut self.state);
            } else if command && z {
                ui::undo(&mut self.ui, &mut self.state);
            } else if command && y {
                ui::redo(&mut self.ui, &mut self.state);
            } else if command && c {
                ui::copy_notes(&mut self.ui, &self.state);
            } else if command && v {
                ui::paste_notes(&mut self.ui, &mut self.state);
            } else if command && a && self.ui.focus == ui::FocusPanel::PianoRoll {
                ui::select_all_notes(&mut self.ui, &self.state);
            } else if command && i_key && self.ui.focus == ui::FocusPanel::PianoRoll {
                ui::invert_note_selection(&mut self.ui, &self.state);
            } else if command && b && self.ui.focus == ui::FocusPanel::PianoRoll {
                ui::duplicate_selected_notes(&mut self.ui, &mut self.state);
            } else if command && l {
                if self.ui.focus == ui::FocusPanel::PianoRoll {
                    ui::legato_selected_notes(&mut self.ui, &mut self.state);
                } else {
                    ui::link_selected_channel_to_insert(&mut self.ui, &mut self.state);
                }
            }
        }
        let pressed = ui.ctx().input(|i| i.pointer.primary_pressed());
        eframe::egui::Panel::top("transport").show(ui, |u| {
            ui::transport(&mut self.ui, &mut self.state, u);
        });
        if self.ui.show_browser {
            eframe::egui::Panel::left("browser").show(ui, |u| {
                eframe::egui::ScrollArea::vertical().show(u, |u| {
                    ui::browser(&mut self.ui, &mut self.state, u);
                });
            });
        }
        eframe::egui::Panel::bottom("hint").show(ui, |u| {
            u.horizontal(|u| {
                u.label(format!("status: {}", self.ui.status));
            });
        });
        eframe::egui::CentralPanel::default().show(ui, |u| {
            eframe::egui::ScrollArea::vertical().show(u, |u| {
                ui::tutorial(&mut self.ui, &mut self.state, u);
                if self.ui.show_rack {
                    let r = u
                        .scope(|u| ui::channel_rack(&mut self.ui, &mut self.state, u))
                        .response;
                    if pressed && r.hovered() {
                        self.ui.focus = ui::FocusPanel::Rack;
                    }
                    u.separator();
                }
                if self.ui.show_piano {
                    let r = u
                        .scope(|u| ui::piano_roll(&mut self.ui, &mut self.state, u))
                        .response;
                    if pressed && r.hovered() {
                        self.ui.focus = ui::FocusPanel::PianoRoll;
                    }
                    u.separator();
                }
                if self.ui.show_playlist || self.ui.show_mixer {
                    u.horizontal(|u| {
                        if self.ui.show_playlist {
                            let r = u
                                .scope(|u| {
                                    u.vertical(|u| {
                                        ui::playlist(&mut self.ui, &mut self.state, u);
                                    });
                                })
                                .response;
                            if pressed && r.hovered() {
                                self.ui.focus = ui::FocusPanel::Playlist;
                            }
                        }
                        if self.ui.show_playlist && self.ui.show_mixer {
                            u.separator();
                        }
                        if self.ui.show_mixer {
                            let r = u
                                .scope(|u| {
                                    u.vertical(|u| {
                                        ui::mixer(&mut self.ui, &mut self.state, u);
                                    });
                                })
                                .response;
                            if pressed && r.hovered() {
                                self.ui.focus = ui::FocusPanel::Mixer;
                            }
                        }
                    });
                    u.separator();
                }
            });
        });
        ui::edison_window(&mut self.ui, &mut self.state, ui.ctx());
        let pointer_down = ui.ctx().input(|i| i.pointer.any_down());
        self.ui.record_song(&self.state.song, pointer_down);
        self.state.sync_audio();
    }
}

fn main() {
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
