use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const NO_VALUE: &str = "—";
const HEADER_HEIGHT: f32 = 38.0;
const CELL_PADDING: f32 = 10.0;
const CPU_COLOR: Color32 = Color32::from_rgb(121, 224, 166);
const RAM_COLOR: Color32 = Color32::from_rgb(121, 207, 244);
const READ_COLOR: Color32 = Color32::from_rgb(195, 173, 255);
const WRITE_COLOR: Color32 = Color32::from_rgb(248, 184, 132);

#[derive(Clone, Copy)]
struct DiskIo {
    read: u64,
    write: u64,
}

#[derive(Clone, Copy)]
struct DiskRate {
    read: f64,
    write: f64,
}

impl DiskIo {
    #[cfg(target_os = "linux")]
    fn parse(contents: &str) -> Option<Self> {
        let mut read = None;
        let mut write = None;
        for line in contents.lines() {
            if let Some(value) = line.strip_prefix("read_bytes:") {
                read = value.trim().parse().ok();
            }
            if let Some(value) = line.strip_prefix("write_bytes:") {
                write = value.trim().parse().ok();
            }
        }
        Some(Self {
            read: read?,
            write: write?,
        })
    }

    fn rate_since(self, old: Self, seconds: f64) -> Option<DiskRate> {
        Some(DiskRate {
            read: self.read.checked_sub(old.read)? as f64 / seconds,
            write: self.write.checked_sub(old.write)? as f64 / seconds,
        })
    }
}

#[cfg(target_os = "linux")]
fn disk_io(pid: Pid) -> Option<DiskIo> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    DiskIo::parse(&contents)
}

#[cfg(not(target_os = "linux"))]
fn disk_io(_: Pid) -> Option<DiskIo> {
    None
}

struct ProcessRow {
    pid: Pid,
    title: String,
    cpu: Option<f32>,
    ram: u64,
    disk: Option<DiskRate>,
}

struct Sampler {
    system: System,
    previous: HashMap<(Pid, u64), Option<DiskIo>>,
    sampled_at: Option<Instant>,
}

impl Sampler {
    fn collect(&mut self) -> Vec<ProcessRow> {
        let started = Instant::now();
        let elapsed = self.sampled_at.replace(started).map(|last| started - last);
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .without_tasks(),
        );
        let mut current = HashMap::with_capacity(self.system.processes().len());
        let mut rows = Vec::with_capacity(current.capacity());

        for (&pid, process) in self.system.processes() {
            let key = (pid, process.start_time());
            let previous = self.previous.get(&key);
            let counters = disk_io(pid);
            current.insert(key, counters);

            let disk = match (counters, previous, elapsed) {
                (Some(now), Some(Some(old)), Some(elapsed)) if !elapsed.is_zero() => {
                    now.rate_since(*old, elapsed.as_secs_f64())
                }
                _ => None,
            };
            rows.push(ProcessRow {
                pid,
                title: process.name().to_string_lossy().into_owned(),
                cpu: previous.map(|_| process.cpu_usage()),
                ram: process.memory(),
                disk,
            });
        }

        self.previous = current;
        rows
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Title,
    Cpu,
    Ram,
    Storage,
}

impl SortBy {
    const COLUMNS: [(Self, &'static str, &'static str); 4] = [
        (Self::Title, "Title", "Process name and PID"),
        (Self::Cpu, "CPU usage", "100% is one logical CPU"),
        (Self::Ram, "RAM usage", "Resident memory (RSS)"),
        (
            Self::Storage,
            "Storage usage",
            "Disk reads and writes per second, where available",
        ),
    ];

    fn header_color(self, ui: &egui::Ui) -> Color32 {
        match self {
            Self::Title => ui.visuals().strong_text_color(),
            Self::Cpu => CPU_COLOR,
            Self::Ram => RAM_COLOR,
            Self::Storage => READ_COLOR,
        }
    }

    fn compare(self, a: &ProcessRow, b: &ProcessRow) -> std::cmp::Ordering {
        match self {
            Self::Title => a.title.cmp(&b.title),
            Self::Cpu => a
                .cpu
                .partial_cmp(&b.cpu)
                .unwrap_or(std::cmp::Ordering::Equal),
            Self::Ram => a.ram.cmp(&b.ram),
            Self::Storage => {
                let total = |p: &ProcessRow| p.disk.map(|io| io.read + io.write);
                total(a)
                    .partial_cmp(&total(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }
        }
    }
}

pub struct ProcessTable {
    request: Sender<()>,
    snapshots: Receiver<Option<Vec<ProcessRow>>>,
    rows: Option<Vec<ProcessRow>>,
    next_refresh: Instant,
    refreshing: bool,
    unavailable: bool,
    filter: String,
    sort: SortBy,
    descending: bool,
}

impl ProcessTable {
    pub fn new(ctx: egui::Context) -> Self {
        let (request, requests) = mpsc::channel();
        let (snapshot, snapshots) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok(system) = System::new() else {
                let _ = snapshot.send(None);
                ctx.request_repaint();
                return;
            };
            let mut sampler = Sampler {
                system,
                previous: HashMap::new(),
                sampled_at: None,
            };
            while requests.recv().is_ok() {
                if snapshot.send(Some(sampler.collect())).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self {
            request,
            snapshots,
            rows: None,
            next_refresh: Instant::now(),
            refreshing: false,
            unavailable: false,
            filter: String::new(),
            sort: SortBy::Cpu,
            descending: true,
        }
    }

    fn update(&mut self, ctx: &egui::Context) {
        if let Ok(rows) = self.snapshots.try_recv() {
            self.unavailable = rows.is_none();
            self.rows = rows;
            self.refreshing = false;
        }
        if self.refreshing || self.unavailable {
            return;
        }

        let now = Instant::now();
        if now >= self.next_refresh {
            self.refreshing = self.request.send(()).is_ok();
            self.unavailable = !self.refreshing;
            self.next_refresh = now + UPDATE_INTERVAL;
        }
        ctx.request_repaint_after(self.next_refresh - now);
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.update(ui.ctx());
        let rows = self.rows.as_deref();
        let mut reset_scroll = false;
        let mut filtered = false;
        ui.add_space(4.0);
        let visible = ui
            .horizontal(|ui| {
                let available = ui.available_width();
                let compact = available < 400.0;
                let search_width = (available - if compact { 86.0 } else { 155.0 }).min(340.0);
                reset_scroll = process_filter(ui, &mut self.filter, search_width, compact);

                let filter = self.filter.trim().to_lowercase();
                filtered = !filter.is_empty();
                rows.map(|rows| {
                    let visible: Vec<_> = rows
                        .iter()
                        .filter(|process| {
                            !filtered
                                || process.title.to_lowercase().contains(&filter)
                                || process.pid.to_string() == filter
                        })
                        .collect();
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(CELL_PADDING);
                        let count = if filtered {
                            let count = format!("{} of {}", visible.len(), rows.len());
                            if !compact {
                                ui.weak("shown");
                            }
                            count
                        } else {
                            ui.weak(if rows.len() == 1 {
                                "process"
                            } else {
                                "processes"
                            });
                            rows.len().to_string()
                        };
                        ui.label(RichText::new(count).strong())
                            .on_hover_text(format!(
                                "{} of {} processes shown",
                                visible.len(),
                                rows.len()
                            ));
                    });
                    visible
                })
            })
            .inner;
        ui.add_space(10.0);

        let Some(mut visible) = visible else {
            ui.weak(if self.unavailable {
                "Process data is unavailable on this system."
            } else {
                "Loading processes…"
            });
            return;
        };

        visible.sort_unstable_by(|a, b| {
            let order = self.sort.compare(a, b);
            let order = if self.descending {
                order.reverse()
            } else {
                order
            };
            order.then_with(|| a.pid.cmp(&b.pid))
        });

        let mut next_sort = None;
        // Keep the four columns intact when the native window is narrow.
        let table_width = ui.available_width().max(630.0);
        egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(table_width);
            ui.spacing_mut().item_spacing.y = 1.0;
            let header_rect = egui::Rect::from_min_size(
                ui.cursor().min,
                egui::vec2(table_width, HEADER_HEIGHT),
            );
            ui.painter().rect_filled(header_rect, 5.0, Color32::from_rgb(29, 34, 43));
            ui.painter().hline(
                header_rect.x_range().shrink(5.0),
                header_rect.bottom() - 0.5,
                egui::Stroke::new(1.0, Color32::from_rgb(61, 71, 87)),
            );
            let mut table = TableBuilder::new(ui)
                .striped(true)
                .cell_layout(egui::Layout::left_to_right(Align::Center))
                .column(Column::remainder().at_least(118.0).clip(true))
                .column(Column::exact((table_width * 0.14).max(120.0)))
                .column(Column::exact((table_width * 0.14).max(120.0)))
                .column(Column::exact((table_width * 0.24).max(230.0)));
            if reset_scroll {
                table = table.scroll_to_row(0, Some(Align::TOP));
            }
            table.header(HEADER_HEIGHT, |mut header| {
                for (column, label, hint) in SortBy::COLUMNS {
                    header.col(|ui| {
                        ui.add_space(4.0);
                        let active = self.sort == column;
                        let label = if active {
                            let arrow = if self.descending { "↓" } else { "↑" };
                            format!("{label}  {arrow}")
                        } else {
                            label.to_owned()
                        };
                        let text = RichText::new(label).strong().color(column.header_color(ui));
                        let button = egui::Button::new(text)
                            .min_size(egui::vec2(0.0, 28.0))
                            .frame_when_inactive(active);
                        if ui.add(button).on_hover_text(hint).clicked() {
                            next_sort = Some(column);
                        }
                    });
                }
            }).body(|body| {
                body.rows(24.0, visible.len(), |mut row| {
                    let process = visible[row.index()];
                    row.col(|ui| {
                        ui.add_space(CELL_PADDING);
                        let mut title = egui::text::LayoutJob::default();
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        title.append(&process.title, 0.0, egui::TextFormat::simple(font.clone(), ui.visuals().text_color()));
                        title.append(&format!("  ·  {}", process.pid), 0.0, egui::TextFormat::simple(font, ui.visuals().weak_text_color()));
                        ui.add(egui::Label::new(title).truncate())
                            .on_hover_text(format!("{} · PID {}", process.title, process.pid));
                    });
                    row.col(|ui| {
                        ui.add_space(CELL_PADDING);
                        if let Some(cpu) = process.cpu {
                            let mut text = RichText::new(format!("{cpu:.1}%")).monospace();
                            text = if cpu >= 80.0 {
                                text.color(Color32::from_rgb(255, 142, 136))
                            } else if cpu >= 30.0 {
                                text.color(WRITE_COLOR)
                            } else if cpu > 0.0 {
                                text.color(CPU_COLOR)
                            } else {
                                text.weak()
                            };
                            ui.label(text);
                        } else {
                            ui.weak(NO_VALUE);
                        }
                    });
                    row.col(|ui| {
                        ui.add_space(CELL_PADDING);
                        ui.label(RichText::new(format_bytes(process.ram as f64)).monospace().color(RAM_COLOR));
                    });
                    row.col(|ui| {
                        ui.add_space(CELL_PADDING);
                        if let Some(disk) = process.disk {
                            for (arrow, rate, color, hint) in [
                                ("↓", disk.read, READ_COLOR, "Disk reads per second"),
                                ("↑", disk.write, WRITE_COLOR, "Disk writes per second"),
                            ] {
                                let text = RichText::new(format!("{arrow} {}/s", format_bytes(rate))).monospace();
                                ui.label(if rate > 0.0 { text.color(color) } else { text.weak() }).on_hover_text(hint);
                            }
                        } else {
                            ui.weak(NO_VALUE).on_hover_text("Disk activity is unavailable or waiting for a second sample.");
                        }
                    });
                });
            });
            if visible.is_empty() {
                ui.weak(if filtered { "No processes match the filter." } else { "No processes available." });
            }
        });

        if let Some(column) = next_sort {
            self.descending = if column == self.sort {
                !self.descending
            } else {
                column != SortBy::Title
            };
            self.sort = column;
            ui.ctx().request_repaint();
        }
    }
}

fn process_filter(ui: &mut egui::Ui, query: &mut String, width: f32, compact: bool) -> bool {
    let mut frame = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(9, 3))
        .fill(Color32::from_rgb(22, 26, 33))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(55, 65, 80)))
        .corner_radius(7)
        .begin(ui);
    let (changed, highlighted) = frame
        .content_ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            let (icon, _) = ui.allocate_exact_size(egui::vec2(15.0, 28.0), egui::Sense::hover());
            let center = icon.center() - egui::vec2(1.5, 1.5);
            let stroke = egui::Stroke::new(1.4, ui.visuals().text_color());
            ui.painter().circle_stroke(center, 4.0, stroke);
            ui.painter().line_segment(
                [center + egui::vec2(3.0, 3.0), center + egui::vec2(6.1, 6.1)],
                stroke,
            );

            let input_width = width - 73.0;
            let search = ui.add(
                egui::TextEdit::singleline(query)
                    .hint_text(if compact {
                        "Name or PID"
                    } else {
                        "Search by process name or PID"
                    })
                    .desired_width(input_width)
                    .min_size(egui::vec2(input_width, 28.0))
                    .align(egui::Align2::LEFT_CENTER)
                    .margin(0)
                    .frame(egui::Frame::NONE),
            );
            let mut changed = search.changed();
            if query.is_empty() {
                ui.add_space(24.0);
            } else if ui
                .add_sized(
                    [24.0, 26.0],
                    egui::Button::new("×").frame_when_inactive(false),
                )
                .on_hover_text("Clear process search")
                .clicked()
            {
                query.clear();
                search.request_focus();
                changed = true;
            }
            (changed, search.has_focus())
        })
        .inner;
    if highlighted {
        frame.frame.stroke.color = Color32::from_rgb(102, 157, 186);
    }
    frame.end(ui);
    changed
}

pub(crate) fn format_bytes(bytes: f64) -> String {
    let mut value = bytes;
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if value < 1024.0 {
            return if unit == "B" {
                format!("{value:.0} {unit}")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} TiB")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn disk_rate_uses_storage_counters_and_elapsed_time() {
        let before = DiskIo::parse("rchar: 99999\nread_bytes: 100\nwrite_bytes: 200\n").unwrap();
        let after = DiskIo::parse("rchar: 999999\nread_bytes: 500\nwrite_bytes: 1200\n").unwrap();

        let rate = after.rate_since(before, 2.0).unwrap();
        assert_eq!((rate.read, rate.write), (200.0, 500.0));
    }

    #[test]
    fn missing_or_reset_counters_do_not_report_idle_disk() {
        assert!(DiskIo::parse("read_bytes: 100\n").is_none());

        let previous = DiskIo {
            read: 100,
            write: 200,
        };
        let reset = DiskIo {
            read: 0,
            write: 200,
        };
        assert!(reset.rate_since(previous, 1.0).is_none());
    }
}
