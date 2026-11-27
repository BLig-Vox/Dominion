use eframe::egui;
use shadcn_egui::components::tabs::{Tabs, TabsVariant};
use shadcn_egui::style::{AppVariant, Paint, Radius, Role};

const TAB_LABELS: [&str; 3] = ["Tab 1", "Tab 2", "Tab 3"];
const NOTEBOOK_TABS: AppVariant<TabsVariant> = TabsVariant::app("notebook");

#[derive(Default)]
struct Dominion {
    startup_window_ready: bool,
    active_tab: usize,
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
                ui.add(
                    Tabs::new(&mut self.active_tab, &TAB_LABELS)
                        .variant(NOTEBOOK_TABS)
                        .width(228.0),
                );
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.centered_and_justified(|ui| {
                ui.heading(if self.active_tab == 0 {
                    "Hello World"
                } else {
                    TAB_LABELS[self.active_tab]
                });
            });
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_maximized(true)
            .with_min_inner_size([240.0, 160.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Dominion",
        options,
        Box::new(|creation_context| {
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
            Ok(Box::new(Dominion::default()))
        }),
    )
}
