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
mod ui_widgets;

use engine::{AppState, start_audio};
use raw_window_handle::HasWindowHandle;
use ui::AppUi;

struct DawApp {
    _stream: Option<cpal::Stream>,
    state: AppState,
    ui: AppUi,
}

impl eframe::App for DawApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        let active_repaint = self.state.rt.playing
            || self.state.rt.recording
            || ui.ctx().input(|i| i.pointer.any_down());
        if active_repaint {
            ui.ctx().request_repaint();
        } else {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(80));
        }
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

        let top_frame = eframe::egui::Frame::new()
            .fill(ui_widgets::BG_PANEL)
            .inner_margin(eframe::egui::Margin::symmetric(8, 4))
            .stroke(eframe::egui::Stroke::new(1.0, ui_widgets::BORDER_SUBTLE));
        eframe::egui::Panel::top("transport")
            .frame(top_frame)
            .show(ui, |u| {
                ui::transport(&mut self.ui, &mut self.state, u);
            });
        if self.ui.show_browser {
            let side_frame = eframe::egui::Frame::new()
                .fill(ui_widgets::BG_PANEL)
                .inner_margin(eframe::egui::Margin::same(6))
                .stroke(eframe::egui::Stroke::new(1.0, ui_widgets::BORDER_SUBTLE));
            eframe::egui::Panel::left("browser")
                .frame(side_frame)
                .default_size(230.0)
                .min_size(180.0)
                .max_size(340.0)
                .show(ui, |u| {
                    eframe::egui::ScrollArea::vertical().show(u, |u| {
                        ui::browser(&mut self.ui, &mut self.state, u);
                    });
                });
        }
        let bot_frame = eframe::egui::Frame::new()
            .fill(ui_widgets::BG_DEEP)
            .inner_margin(eframe::egui::Margin::symmetric(8, 3))
            .stroke(eframe::egui::Stroke::new(1.0, ui_widgets::BORDER_SUBTLE));
        eframe::egui::Panel::bottom("hint")
            .frame(bot_frame)
            .show(ui, |u| {
                u.horizontal(|u| {
                    let audio_ok = self.state.audio.is_some();
                    u.colored_label(
                        if audio_ok {
                            ui_widgets::ACCENT_GREEN
                        } else {
                            ui_widgets::ACCENT_AMBER
                        },
                        if audio_ok {
                            "● Audio"
                        } else {
                            "○ No Audio"
                        },
                    );
                    u.separator();
                    if self.ui.typing_piano {
                        u.colored_label(
                            ui_widgets::ACCENT_BLUE,
                            format!("[Keys C{}]", self.ui.typing_octave),
                        );
                        u.separator();
                    }
                    let ch_name = self
                        .state
                        .song
                        .channels
                        .get(self.ui.sel_ch)
                        .map(|c| c.name.as_str())
                        .unwrap_or("-");
                    u.label(
                        eframe::egui::RichText::new(format!(
                            "Ch: {ch_name}  |  P{}  |  {:.0} BPM",
                            self.ui.sel_pat + 1,
                            self.state.song.bpm
                        ))
                        .color(ui_widgets::TEXT_MUTED)
                        .small(),
                    );
                    u.separator();
                    u.label(
                        eframe::egui::RichText::new(&self.ui.status)
                            .color(ui_widgets::TEXT_PRIMARY),
                    );
                });
            });
        eframe::egui::CentralPanel::default()
            .frame(eframe::egui::Frame::new().fill(ui_widgets::BG_DEEP))
            .show(ui, |u| {
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
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 840.0])
            .with_min_inner_size([980.0, 640.0])
            .with_icon(std::sync::Arc::new(ui_widgets::build_app_icon())),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "Thunder",
        options,
        Box::new(move |cc| {
            ui_widgets::apply_studio_theme(&cc.egui_ctx);
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

#[cfg(test)]
mod tests {
    #[test]
    fn windows_installer_places_shortcuts_and_icons() {
        let wxs = include_str!("../wix/main.wxs");

        let start_menu_idx = wxs
            .find("Id='StartMenuThunder'")
            .expect("StartMenuThunder shortcut missing");
        let start_menu_tag =
            &wxs[start_menu_idx..wxs[start_menu_idx..].find("/>").unwrap() + start_menu_idx];
        assert!(
            start_menu_tag.contains("Directory='ApplicationProgramsFolder'"),
            "StartMenuThunder must specify Directory='ApplicationProgramsFolder'"
        );
        assert!(
            start_menu_tag.contains("Icon='Thunder.exe'"),
            "StartMenuThunder must specify Icon='Thunder.exe'"
        );

        let desktop_idx = wxs
            .find("Id='DesktopThunder'")
            .expect("DesktopThunder shortcut missing");
        let desktop_tag = &wxs[desktop_idx..wxs[desktop_idx..].find("/>").unwrap() + desktop_idx];
        assert!(
            desktop_tag.contains("Directory='DesktopFolder'"),
            "DesktopThunder must specify Directory='DesktopFolder'"
        );
        assert!(
            desktop_tag.contains("Icon='Thunder.exe'"),
            "DesktopThunder must specify Icon='Thunder.exe'"
        );

        assert!(
            wxs.contains("<Icon Id='Thunder.exe' SourceFile='assets\\icon.ico'/>"),
            "wix/main.wxs must define Thunder.exe Icon from assets\\icon.ico"
        );
        assert!(
            wxs.contains("<Property Id='ARPPRODUCTICON' Value='Thunder.exe'/>"),
            "wix/main.wxs must set ARPPRODUCTICON to Thunder.exe"
        );

        let ico = include_bytes!("../assets/icon.ico");
        assert!(
            ico.len() >= 4 && ico[..4] == [0, 0, 1, 0],
            "assets/icon.ico must have a valid Windows ICO header"
        );
    }
}
