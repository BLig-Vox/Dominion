mod processes;
#[cfg(target_os = "linux")]
mod storage;

use eframe::egui;
use shadcn_egui::components::tabs::{Tabs, TabsVariant};
use shadcn_egui::style::{AppVariant, Paint, Radius, Role};

const TAB_LABELS: [&str; 3] = ["Processes", "Storage", "Tab 3"];
const NOTEBOOK_TABS: AppVariant<TabsVariant> = TabsVariant::app("notebook");
const LOGO: &[u8] = include_bytes!("../assets/dominion.png");

struct Dominion {
    startup_window_ready: bool,
    active_tab: usize,
    logo: egui::TextureHandle,
    processes: processes::ProcessTable,
    #[cfg(target_os = "linux")]
    storage: storage::Storage,
}

impl eframe::App for Dominion {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.startup_window_ready && ui.input(|input| input.viewport().focused == Some(true)) {
            self.startup_window_ready = true;
            // X11 window managers can ignore maximize requests before the window is visible.
            if ui.input(|input| input.viewport().maximized == Some(false)) {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            }
        }

        egui::Panel::top("notebook_tabs")
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 8,
                bottom: 0,
            }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.image((self.logo.id(), egui::vec2(24.0, 24.0)));
                    ui.add(
                        Tabs::new(&mut self.active_tab, &TAB_LABELS)
                            .variant(NOTEBOOK_TABS)
                            .width(228.0),
                    );
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            if self.active_tab == 0 {
                self.processes.show(ui);
            } else if self.active_tab == 1 {
                #[cfg(target_os = "linux")]
                self.storage.show(ui);
                #[cfg(not(target_os = "linux"))]
                ui.weak("Storage inventory is available on Linux.");
            } else {
                ui.centered_and_justified(|ui| {
                    ui.heading(TAB_LABELS[self.active_tab]);
                });
            }
        });
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => run_desktop()?,
        [flag] if flag == "--version" || flag == "-V" => {
            println!("Dominion {}", env!("CARGO_PKG_VERSION"));
        }
        [flag] if flag == "--help" || flag == "-h" => {
            println!(
                "Usage: dominion [--desktop | --update [--stable | --edge] | --version]\n\n\
                 With no options, open the app.\n\
                 --desktop  Add Dominion to the Linux app menu.\n\
                 --update   Update this installation from GitHub."
            );
        }
        #[cfg(target_os = "linux")]
        [flag] if flag == "--print-icon" => {
            use std::io::Write;
            std::io::stdout().lock().write_all(LOGO)?;
        }
        #[cfg(target_os = "linux")]
        [flag, ..] if flag == "--desktop" || flag == "--update" => {
            let executable = std::env::current_exe()?;
            let mut installer = std::process::Command::new("sh");
            installer
                .args(["-c", include_str!("../install.sh"), "dominion"])
                .env("DOMINION_EXECUTABLE", executable);
            if flag == "--desktop" && args.len() == 1 {
                installer.arg("--register-only");
            }
            let status = installer.args(args).status()?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        _ => return Err("Unsupported arguments. Run dominion --help.".into()),
    }
    Ok(())
}

fn run_desktop() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(LOGO).expect("valid Dominion icon");
    let logo = egui::ColorImage::from_rgba_unmultiplied(
        [icon.width as usize, icon.height as usize],
        &icon.rgba,
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("dominion")
            .with_icon(icon)
            .with_maximized(true)
            .with_min_inner_size([240.0, 160.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Dominion",
        options,
        Box::new(move |creation_context| {
            creation_context.egui_ctx.set_theme(egui::Theme::Dark);
            shadcn_egui::Theme::vega()
                .variant(NOTEBOOK_TABS, TabsVariant::Default, |recipes, _| {
                    let list = &mut recipes.tabs_list;
                    list.pad_top.set(0.0);
                    list.pad_bottom.set(0.0);
                    list.pad_left.set(0.0);
                    list.pad_right.set(0.0);
                    list.gap.set(1.0);
                    list.radius.set(Radius::Px(0.0));
                    list.bg.set(Paint::Role(Role::Transparent, 1.0));

                    let tab = &mut recipes.tabs_trigger;
                    tab.height_inset.set(0.0);
                    tab.radius.set(Radius::Px(0.0));
                    tab.radius_top.set(Radius::Px(6.0));
                    tab.bg.set(Paint::Role(Role::Muted, 1.0));
                })
                .apply(&creation_context.egui_ctx);
            Ok(Box::new(Dominion {
                startup_window_ready: false,
                active_tab: 0,
                logo: creation_context.egui_ctx.load_texture(
                    "Dominion logo",
                    logo,
                    egui::TextureOptions::LINEAR,
                ),
                processes: processes::ProcessTable::new(creation_context.egui_ctx.clone()),
                #[cfg(target_os = "linux")]
                storage: storage::Storage::default(),
            }))
        }),
    )
}
