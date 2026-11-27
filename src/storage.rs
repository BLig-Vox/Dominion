mod cache;
mod linux;

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Align, Color32, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::processes::format_bytes;
use cache::{Cache, FolderKey};
use linux::{FILE_CUTOFFS, Inventory, Scan, Volume};

const TEAL: Color32 = Color32::from_rgb(112, 214, 196);
const AMBER: Color32 = Color32::from_rgb(246, 184, 116);
const RED: Color32 = Color32::from_rgb(244, 131, 139);
const HEADER: Color32 = Color32::from_gray(31);
const ROW: Color32 = Color32::from_gray(21);
const MUTED: Color32 = Color32::from_gray(164);
const TEXT: Color32 = Color32::from_gray(236);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Device,
    Mount,
    Filesystem,
    Total,
    Available,
    Used,
}

impl Sort {
    const COLUMNS: [(Self, &'static str); 6] = [
        (Self::Device, "Device"),
        (Self::Mount, "Mount point"),
        (Self::Filesystem, "File system"),
        (Self::Total, "Capacity"),
        (Self::Available, "Available"),
        (Self::Used, "Used"),
    ];

    fn compare(self, a: &Volume, b: &Volume, reverse: bool) -> Ordering {
        fn known_first<T: PartialOrd>(a: Option<T>, b: Option<T>, reverse: bool) -> Ordering {
            match (a, b) {
                (Some(a), Some(b)) if reverse => b.partial_cmp(&a).unwrap_or(Ordering::Equal),
                (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        }
        match self {
            Self::Device => known_first(Some(&a.source), Some(&b.source), reverse),
            Self::Mount => known_first(a.path.as_ref(), b.path.as_ref(), reverse),
            Self::Filesystem => known_first(Some(&a.filesystem), Some(&b.filesystem), reverse),
            Self::Total => known_first(a.total, b.total, reverse),
            Self::Available => known_first(a.available, b.available, reverse),
            Self::Used => known_first(a.used_fraction(), b.used_fraction(), reverse),
        }
    }
}

#[derive(PartialEq, Eq)]
struct Selection {
    source: String,
    mount: Option<PathBuf>,
}

impl From<&Volume> for Selection {
    fn from(volume: &Volume) -> Self {
        Self {
            source: volume.source.clone(),
            mount: volume.path.clone(),
        }
    }
}

impl Selection {
    fn matches(&self, volume: &Volume) -> bool {
        self.source == volume.source && self.mount == volume.path
    }
}

type Job<T> = Receiver<Result<T, String>>;

struct ScanJob {
    cancel: Arc<AtomicBool>,
    output: Job<Option<Snapshot>>,
    mode: LoadMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadMode {
    CachedOnly,
    ScanIfMissing,
    Refresh,
}

struct Snapshot {
    scan: Scan,
    scanned_at: i64,
    cache_error: Option<String>,
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn load_folder(
    path: &Path,
    cancel: &AtomicBool,
    mode: LoadMode,
) -> std::io::Result<Option<Snapshot>> {
    let folder = FolderKey::read(path)?;
    let cache = Cache::open_default();
    if mode != LoadMode::Refresh {
        let cached = cache
            .as_ref()
            .map_err(|error| std::io::Error::other(error.to_string()))
            .and_then(|cache| cache.load(&folder));
        match cached {
            Ok(Some((scan, scanned_at))) => {
                return Ok(Some(Snapshot {
                    scan,
                    scanned_at,
                    cache_error: None,
                }));
            }
            result if mode == LoadMode::CachedOnly => return result.map(|_| None),
            _ => {}
        }
    }

    let scanned_at = unix_millis();
    let scan = linux::scan_path(path, cancel)?;
    let cache_error = if cancel.load(AtomicOrdering::Relaxed) {
        None
    } else {
        cache
            .and_then(|cache| cache.save(&folder, &scan, scanned_at))
            .err()
            .map(|error| error.to_string())
    };
    Ok(Some(Snapshot {
        scan,
        scanned_at,
        cache_error,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum View {
    Folders,
    Files,
    Volumes,
}

pub struct Storage {
    view: View,
    inventory: Option<Inventory>,
    inventory_job: Option<Job<Inventory>>,
    inventory_error: Option<String>,
    sort: Sort,
    descending: bool,
    show_system: bool,
    selected: Option<Selection>,
    folder: Option<PathBuf>,
    scan_job: Option<ScanJob>,
    scan: Option<Snapshot>,
    scan_error: Option<String>,
    cutoff: usize,
    large_only: bool,
}

fn background<T: Send + 'static>(
    ctx: &egui::Context,
    task: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Job<T> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("dominion-storage".into())
        .spawn({
            let tx = tx.clone();
            move || {
                let _ = tx.send(task().map_err(|error| error.to_string()));
                ctx.request_repaint();
            }
        })
    {
        let _ = tx.send(Err(error.to_string()));
    }
    rx
}

fn poll<T>(job: &Job<T>) -> Option<Result<T, String>> {
    match job.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Err("The storage task ended unexpectedly.".into())),
    }
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            view: View::Folders,
            inventory: None,
            inventory_job: None,
            inventory_error: None,
            sort: Sort::Mount,
            descending: false,
            show_system: false,
            selected: None,
            folder: None,
            scan_job: None,
            scan: None,
            scan_error: None,
            cutoff: 0,
            large_only: false,
        }
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.cancel_scan();
    }
}

impl Storage {
    fn cancel_scan(&self) {
        if let Some(job) = &self.scan_job {
            job.cancel.store(true, AtomicOrdering::Relaxed);
        }
    }

    fn open_folder(&mut self, ctx: &egui::Context, path: PathBuf, mode: LoadMode) {
        self.cancel_scan();
        if self.folder.as_ref() != Some(&path) {
            self.scan = None;
        }
        self.folder = Some(path.clone());
        self.scan_error = None;
        let cancel = Arc::new(AtomicBool::new(false));
        let output = background(ctx, {
            let cancel = cancel.clone();
            move || load_folder(&path, &cancel, mode)
        });
        self.scan_job = Some(ScanJob {
            cancel,
            output,
            mode,
        });
    }

    fn update(&mut self, ctx: &egui::Context) {
        if self.inventory.is_none()
            && self.inventory_job.is_none()
            && self.inventory_error.is_none()
        {
            self.inventory_job = Some(background(ctx, linux::inventory));
        }
        if let Some(result) = self.inventory_job.as_ref().and_then(poll) {
            self.inventory_job = None;
            match result {
                Ok(inventory) => {
                    if self.selected.as_ref().is_some_and(|selection| {
                        !inventory
                            .volumes
                            .iter()
                            .any(|volume| selection.matches(volume))
                    }) {
                        self.cancel_scan();
                        self.scan_job = None;
                        self.folder = None;
                        self.selected = None;
                        self.scan = None;
                        self.scan_error = None;
                    }
                    if self.selected.is_none()
                        && let Some(volume) = inventory
                            .volumes
                            .iter()
                            .filter(|volume| volume.scannable && !volume.system)
                            .min_by_key(|volume| &volume.path)
                    {
                        self.select(ctx, Selection::from(volume));
                    }
                    self.inventory = Some(inventory);
                    self.inventory_error = None;
                }
                Err(error) => self.inventory_error = Some(error),
            }
        }
        if let Some(job) = &self.scan_job
            && let Some(result) = poll(&job.output)
        {
            if !job.cancel.load(AtomicOrdering::Relaxed) {
                match result {
                    Ok(scan) => self.scan = scan,
                    Err(error) => self.scan_error = Some(error),
                }
            }
            self.scan_job = None;
        }
    }

    fn select(&mut self, ctx: &egui::Context, selection: Selection) {
        if self.selected.as_ref() != Some(&selection) {
            self.cancel_scan();
            self.scan_job = None;
            self.scan = None;
            self.scan_error = None;
            self.folder = None;
            if let Some(path) = selection.mount.clone() {
                self.open_folder(ctx, path, LoadMode::CachedOnly);
            }
            self.selected = Some(selection);
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.update(ui.ctx());
        let style = ui.style_mut();
        style.interaction.selectable_labels = false;
        style.visuals.selection.bg_fill = Color32::from_rgb(24, 60, 57);
        style.visuals.selection.stroke.color = Color32::from_rgb(222, 246, 241);
        style.visuals.override_text_color = Some(TEXT);
        egui::Frame::new().inner_margin(12).show(ui, |ui| {
            ui.set_width(ui.available_width());
            self.show_source(ui);
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                for (view, label) in [
                    (View::Folders, "Folders"),
                    (View::Files, "Files"),
                    (View::Volumes, "File systems"),
                ] {
                    ui.selectable_value(&mut self.view, view, label);
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt((self.view, self.folder.as_deref()))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.view == View::Volumes {
                        self.show_inventory(ui);
                    } else {
                        self.show_details(ui);
                    }
                });
        });
    }

    fn show_source(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.inventory_error {
            ui.colored_label(RED, format!("Could not read file systems · {error}"));
        }
        let mut next_selection = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Storage").size(21.0).strong());
            if let Some(inventory) = &self.inventory {
                let display = self.selected.as_ref().map(|selection| {
                    selection.mount.as_ref().map_or_else(
                        || selection.source.clone(),
                        |path| path.display().to_string(),
                    )
                });
                egui::ComboBox::from_id_salt("storage_source")
                    .width((ui.available_width() - 12.0).clamp(64.0, 160.0))
                    .selected_text(display.as_deref().unwrap_or("Choose a volume"))
                    .show_ui(ui, |ui| {
                        for volume in inventory.volumes.iter().filter(|volume| {
                            volume.path.is_some() && (self.show_system || !volume.system)
                        }) {
                            let path = volume.path.as_ref().unwrap();
                            let label = format!("{}  ·  {}", path.display(), volume.filesystem);
                            let selected = self
                                .selected
                                .as_ref()
                                .is_some_and(|selection| selection.matches(volume));
                            if ui.selectable_label(selected, label).clicked() {
                                next_selection = Some(Selection::from(volume));
                            }
                        }
                    });
            }
            if ui
                .add_enabled(
                    self.inventory_job.is_none(),
                    egui::Button::new("↻").min_size(egui::vec2(30.0, 0.0)),
                )
                .on_hover_text("Refresh volumes")
                .clicked()
            {
                self.inventory_job = Some(background(ui.ctx(), linux::inventory));
            }
            if self.inventory_job.is_some() {
                ui.spinner();
            }
            if let Some(volume) = self.selected_volume()
                && let (Some(available), Some(total), Some(fraction)) =
                    (volume.available, volume.total, volume.used_fraction())
            {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(
                            "{} free of {} · {:.0}% used",
                            format_bytes(available as f64),
                            format_bytes(total as f64),
                            fraction * 100.0
                        ))
                        .color(MUTED),
                    )
                    .extend(),
                );
            }
        });
        if let Some(selection) = next_selection {
            self.select(ui.ctx(), selection);
        }
    }

    fn show_inventory(&mut self, ui: &mut egui::Ui) {
        let Some(inventory) = &self.inventory else {
            return;
        };
        ui.checkbox(&mut self.show_system, "Include system mounts");
        if let Some(error) = &inventory.device_error {
            ui.colored_label(AMBER, format!("Block devices unavailable · {error}"));
        }
        let mut visible: Vec<_> = inventory
            .volumes
            .iter()
            .filter(|volume| self.show_system || !volume.system)
            .collect();
        visible.sort_by(|a, b| {
            self.sort
                .compare(a, b, self.descending)
                .then(a.source.cmp(&b.source))
        });
        let (selection, sort) = volume_table(
            ui,
            &visible,
            self.selected.as_ref(),
            self.sort,
            self.descending,
        );
        if let Some(sort) = sort {
            self.descending = if self.sort == sort {
                !self.descending
            } else {
                matches!(sort, Sort::Total | Sort::Available | Sort::Used)
            };
            self.sort = sort;
        }
        if let Some(selection) = selection {
            self.select(ui.ctx(), selection);
        }
    }

    fn selected_volume(&self) -> Option<&Volume> {
        let selection = self.selected.as_ref()?;
        self.inventory
            .as_ref()?
            .volumes
            .iter()
            .find(|volume| selection.matches(volume))
    }

    fn show_details(&mut self, ui: &mut egui::Ui) {
        let Some(volume) = self.selected_volume() else {
            return;
        };
        let Some(mount) = volume.path.as_deref() else {
            ui.label(
                RichText::new(format!("{}  ·  {}", volume.source, volume.status)).color(MUTED),
            );
            return;
        };
        if !volume.scannable {
            ui.label(RichText::new("Choose a mounted directory to scan.").color(MUTED));
            return;
        }
        let folder = self.folder.as_deref().unwrap_or(mount).to_path_buf();
        let parent = folder.parent().filter(|parent| parent.starts_with(mount));
        let busy = self.scan_job.is_some();
        let mut next_folder = None;
        ui.horizontal_wrapped(|ui| {
            if let Some(parent) = parent
                && ui
                    .add_enabled(!busy, egui::Button::new("↑ Parent"))
                    .clicked()
            {
                next_folder = Some((parent.to_path_buf(), LoadMode::ScanIfMissing));
            }
            if let Some(job) = &self.scan_job {
                ui.spinner();
                if job.mode != LoadMode::CachedOnly && ui.button("Cancel").clicked() {
                    self.cancel_scan();
                }
                let status = if job.cancel.load(AtomicOrdering::Relaxed) {
                    "Cancelling…"
                } else if job.mode == LoadMode::Refresh {
                    "Scanning…"
                } else {
                    "Loading folder…"
                };
                ui.weak(status);
            } else {
                let label = if self.scan.is_some() {
                    "Rescan folder"
                } else {
                    "Scan folder"
                };
                if ui.button(label).clicked() {
                    next_folder = Some((folder.clone(), LoadMode::Refresh));
                }
            }
            ui.add(
                egui::Label::new(RichText::new(folder.display().to_string()).monospace()).wrap(),
            );
            if let Some(snapshot) = &self.scan {
                let minutes = unix_millis().saturating_sub(snapshot.scanned_at).max(0) / 60_000;
                let age = match minutes {
                    0 => "Scanned just now".to_string(),
                    1..60 => format!("Scanned {minutes}m ago"),
                    60..1440 => format!("Scanned {}h ago", minutes / 60),
                    _ => format!("Scanned {}d ago", minutes / 1440),
                };
                ui.add(egui::Label::new(RichText::new(age).color(MUTED)).extend());
                ui.ctx().request_repaint_after(Duration::from_secs(60));
            }
        });
        ui.add_space(10.0);
        if let Some((path, mode)) = next_folder {
            self.open_folder(ui.ctx(), path, mode);
            return;
        }
        if let Some(error) = &self.scan_error {
            ui.colored_label(RED, format!("Could not load folder · {error}"));
        }
        match &self.scan {
            Some(snapshot) => {
                let scan = &snapshot.scan;
                if let Some(error) = &snapshot.cache_error {
                    ui.colored_label(AMBER, format!("Could not save scan · {error}"));
                }
                if scan.skipped != 0 {
                    ui.colored_label(AMBER, format!("{} entries could not be read", scan.skipped));
                }
                let is_file_view = self.view == View::Files;
                let cutoff = is_file_view
                    .then(|| file_filter(ui, scan, &mut self.cutoff, &mut self.large_only));
                let entries = if is_file_view {
                    let visible = if self.large_only {
                        scan.files
                            .partition_point(|(size, _)| *size >= FILE_CUTOFFS[self.cutoff])
                    } else {
                        scan.files.len()
                    };
                    &scan.files[..visible]
                } else {
                    &scan.folders
                };
                if entries.is_empty() {
                    ui.weak(match (self.view, self.large_only, scan.file_count) {
                        (View::Folders, _, _) => "No subfolders.",
                        (View::Files, true, count) if count > 0 => "No files meet this threshold.",
                        _ => "No files.",
                    });
                } else if let Some(path) = ranking_table(ui, &folder, entries, scan.total, cutoff)
                    && !busy
                {
                    self.open_folder(ui.ctx(), path, LoadMode::ScanIfMissing);
                }
            }
            None if !busy && self.scan_error.is_none() => {
                ui.weak("Scan this folder to see what uses the most space.");
            }
            None => {}
        }
    }
}

fn file_filter(ui: &mut egui::Ui, scan: &Scan, cutoff: &mut usize, large_only: &mut bool) -> u64 {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Flag files ≥").color(MUTED));
        egui::ComboBox::from_id_salt("file_cutoff")
            .width(90.0)
            .selected_text(format_bytes(FILE_CUTOFFS[*cutoff] as f64))
            .show_ui(ui, |ui| {
                for (index, threshold) in FILE_CUTOFFS.into_iter().enumerate() {
                    ui.selectable_value(cutoff, index, format_bytes(threshold as f64));
                }
            });
        ui.checkbox(large_only, "Flagged only");
        let flagged = scan.large_file_counts[*cutoff];
        let count = if *large_only {
            flagged
        } else {
            scan.file_count
        };
        let limit = scan.files.len();
        let status = if *large_only && count > limit {
            format!("Top {limit} of {count} flagged")
        } else if *large_only {
            format!("{flagged} flagged")
        } else if count > limit {
            format!("Top {limit} of {count} · {flagged} flagged")
        } else {
            format!("{} files · {flagged} flagged", scan.file_count)
        };
        ui.add(egui::Label::new(RichText::new(status).color(MUTED)).extend());
    });
    ui.add_space(6.0);
    FILE_CUTOFFS[*cutoff]
}

fn volume_table(
    ui: &mut egui::Ui,
    volumes: &[&Volume],
    selected: Option<&Selection>,
    sort: Sort,
    descending: bool,
) -> (Option<Selection>, Option<Sort>) {
    let mut next_select = None;
    let mut next_sort = None;
    let width = ui.available_width().max(790.0);
    egui::ScrollArea::horizontal()
        .id_salt("volumes_h")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.set_width(width);
            let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, 34.0));
            ui.painter().rect_filled(rect, 5.0, HEADER);
            TableBuilder::new(ui)
                .striped(true)
                .sense(Sense::click())
                .cell_layout(egui::Layout::left_to_right(Align::Center))
                .vscroll(false)
                .column(Column::initial(width * 0.21).at_least(150.0).clip(true))
                .column(Column::remainder().at_least(150.0).clip(true))
                .column(Column::exact(100.0).clip(true))
                .column(Column::exact(88.0))
                .column(Column::exact(88.0))
                .column(Column::exact((width * 0.18).max(134.0)))
                .header(34.0, |mut header| {
                    for (column, label) in Sort::COLUMNS {
                        header.col(|ui| {
                            let active = sort == column;
                            let text = if active {
                                format!("{label} {}", if descending { "↓" } else { "↑" })
                            } else {
                                label.into()
                            };
                            let text = RichText::new(text).strong();
                            let text = if active { text.color(TEAL) } else { text };
                            if ui
                                .add(egui::Button::new(text).frame_when_inactive(false))
                                .clicked()
                            {
                                next_sort = Some(column);
                            }
                        });
                    }
                })
                .body(|body| {
                    body.rows(31.0, volumes.len(), |mut row| {
                        let volume = volumes[row.index()];
                        row.set_selected(
                            selected.is_some_and(|selection| selection.matches(volume)),
                        );
                        row.col(|ui| {
                            ui.add_space(6.0);
                            clipped(
                                ui,
                                RichText::new(&volume.source).monospace().strong(),
                                &volume.source,
                            );
                        });
                        row.col(|ui| {
                            if let Some(path) = &volume.path {
                                let path = path.to_string_lossy();
                                clipped(ui, RichText::new(path.as_ref()), &path);
                            } else {
                                ui.weak(&volume.status);
                            }
                        });
                        row.col(|ui| {
                            let hint = format!("{} · {}", volume.filesystem, volume.status);
                            clipped(ui, RichText::new(&volume.filesystem), &hint);
                        });
                        row.col(|ui| {
                            measure(ui, volume.total);
                        });
                        row.col(|ui| {
                            measure(ui, volume.available);
                        });
                        row.col(|ui| {
                            if let Some(fraction) = volume.used_fraction() {
                                let color = capacity_color(fraction);
                                let label = format!(
                                    "{} · {:.0}%",
                                    format_bytes(volume.used.unwrap_or_default() as f64),
                                    fraction * 100.0
                                );
                                usage(ui, fraction, color, &label)
                                    .on_hover_text("Percentage of total file-system capacity");
                            } else {
                                ui.weak("—");
                            }
                        });
                        if row.response().clicked() {
                            next_select = Some(Selection::from(volume));
                        }
                    })
                });
        });
    (next_select, next_sort)
}

fn clipped(ui: &mut egui::Ui, text: RichText, hint: &str) {
    ui.add(egui::Label::new(text).truncate())
        .on_hover_text(hint);
}

fn measure(ui: &mut egui::Ui, value: Option<u64>) {
    if let Some(bytes) = value {
        ui.label(RichText::new(format_bytes(bytes as f64)).monospace());
    } else {
        ui.weak("—");
    }
}

fn usage(ui: &mut egui::Ui, fraction: f32, color: Color32, label: &str) -> egui::Response {
    let width = (ui.available_width() - 8.0).max(1.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 26.0), Sense::hover());
    ui.painter().text(
        rect.left_top(),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::monospace(11.0),
        ui.visuals().text_color(),
    );
    let track = egui::Rect::from_min_size(
        rect.left_bottom() - egui::vec2(0.0, 7.0),
        egui::vec2(width, 4.0),
    );
    ui.painter()
        .rect_filled(track, 2.0, ui.visuals().widgets.inactive.bg_fill);
    if fraction > 0.0 {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(track.min, egui::vec2(width * fraction.clamp(0.0, 1.0), 4.0)),
            2.0,
            color,
        );
    }
    response
}

fn capacity_color(fraction: f32) -> Color32 {
    if fraction >= 0.90 {
        RED
    } else if fraction >= 0.75 {
        AMBER
    } else {
        TEAL
    }
}

fn path_menu(response: egui::Response, path: &Path) {
    response
        .on_hover_text(path.display().to_string())
        .context_menu(|ui| {
            if ui.button("Copy path").clicked() {
                ui.ctx().copy_text(path.display().to_string());
                ui.close();
            }
        });
}

fn ranking_table(
    ui: &mut egui::Ui,
    folder: &Path,
    entries: &[(u64, PathBuf)],
    total: u64,
    file_cutoff: Option<u64>,
) -> Option<PathBuf> {
    let mut next = None;
    let folders = file_cutoff.is_none();
    let width = ui.available_width();
    let show_location = !folders && width >= 700.0;
    let show_share = width >= 420.0;
    let header_rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, 30.0));
    ui.painter().rect_filled(header_rect, 0.0, HEADER);
    ui.visuals_mut().faint_bg_color = ROW;
    let mut table = TableBuilder::new(ui)
        .id_salt(("storage_ranking", folders))
        .vscroll(false)
        .striped(true)
        .sense(if folders {
            Sense::click()
        } else {
            Sense::hover()
        })
        .cell_layout(egui::Layout::left_to_right(Align::Center))
        .column(Column::exact(32.0))
        .column(Column::remainder().at_least(48.0).clip(true));
    if show_location {
        table = table.column(Column::exact((width * 0.30).min(390.0)).clip(true));
    }
    table = table.column(Column::exact(96.0));
    if show_share {
        table = table.column(Column::exact((width * 0.16).clamp(76.0, 180.0)));
    }
    table
        .header(30.0, |mut header| {
            header.col(|ui| {
                ui.add_space(8.0);
                ui.weak("#");
            });
            header.col(|ui| {
                ui.label(RichText::new(if folders { "Folder" } else { "File" }).strong());
            });
            if show_location {
                header.col(|ui| {
                    ui.weak("Location");
                });
            }
            header.col(|ui| {
                ui.label(RichText::new("Disk usage ↓").strong());
            });
            if show_share {
                header.col(|ui| {
                    ui.weak("% of scan");
                });
            }
        })
        .body(|body| {
            body.rows(28.0, entries.len(), |mut row| {
                let (size, path) = &entries[row.index()];
                let rank = row.index() + 1;
                let flagged = file_cutoff.is_some_and(|cutoff| *size >= cutoff);
                row.col(|ui| {
                    ui.add_space(8.0);
                    ui.weak(rank.to_string());
                });
                row.col(|ui| {
                    let name = path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy();
                    let text = RichText::new(name);
                    let response = if folders {
                        let label = egui::Label::new(text).truncate().sense(Sense::click());
                        ui.add(label)
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                    } else {
                        ui.add(egui::Label::new(text).truncate())
                    };
                    if folders && response.clicked() {
                        next = Some(path.clone());
                    }
                    path_menu(response, path);
                });
                if show_location {
                    row.col(|ui| {
                        let parent = path.parent().unwrap_or(folder);
                        let relative = parent.strip_prefix(folder).unwrap_or(parent);
                        let label = if relative.as_os_str().is_empty() {
                            ".".into()
                        } else {
                            relative.to_string_lossy()
                        };
                        clipped(
                            ui,
                            RichText::new(label).color(MUTED),
                            &parent.display().to_string(),
                        );
                    });
                }
                row.col(|ui| {
                    let label = format_bytes(*size as f64);
                    if flagged {
                        ui.colored_label(AMBER, format!("{label}  ▲"))
                            .on_hover_text("Meets the large-file threshold");
                    } else {
                        ui.label(label);
                    }
                });
                if show_share {
                    row.col(|ui| {
                        let fraction = *size as f32 / total.max(1) as f32;
                        ui.label(RichText::new(format!("{:.1}%", fraction * 100.0)).color(MUTED));
                        let width = (ui.available_width() - 10.0).max(1.0);
                        let (track, _) =
                            ui.allocate_exact_size(egui::vec2(width, 3.0), Sense::hover());
                        ui.painter().rect_filled(track, 1.5, Color32::from_gray(45));
                        let fill = egui::Rect::from_min_size(
                            track.min,
                            egui::vec2(width * fraction.clamp(0.0, 1.0), 3.0),
                        );
                        ui.painter().rect_filled(fill, 1.5, TEAL);
                    });
                }
                if folders && row.response().clicked() {
                    next = Some(path.clone());
                }
            });
        });
    next
}
