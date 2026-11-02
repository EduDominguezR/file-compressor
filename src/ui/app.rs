//! Ventana única de dos columnas: barra lateral (acciones) + área principal (estadísticas,
//! progreso, consola). Toda la E/S pesada (validación, espacio libre, compresión) ocurre en hilos
//! del motor; aquí solo se dibuja y se leen mensajes del canal.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui::{self, Color32, RichText};

use megacompress::app_state::{Machine, Stage};
use megacompress::engine::cancel::CancelFlag;
use megacompress::engine::limits::Limits;
use megacompress::engine::mode::{system_resources, Config, Mode};
use megacompress::engine::orchestrator::{self, Analysis, Job, Report, SLOW_WARNING};
use megacompress::engine::progress::{Phase, Snapshot};
use megacompress::engine::{Msg, Repaint};
use megacompress::error::Error;
use megacompress::fs::space::{plan, Verdict};
use megacompress::util::{human_duration, human_rate, human_size};

use super::{theme, widgets};

/// Máximo de líneas que conserva la consola (se descartan las más antiguas).
pub const LOG_MAX_LINES: usize = 500;

/// Formato único de las líneas del registro. El corchete de cierre es parte literal del formato
/// (no un carácter que dependa de la anchura del número) para que nunca quede huérfano.
fn format_log_line(elapsed: Duration, text: &str) -> String {
    let secs = elapsed.as_secs_f64();
    format!("[{secs:>7.1}s] {text}")
}

pub struct MegaApp {
    machine: Machine,
    path: Option<PathBuf>,
    dest: Option<PathBuf>,
    dest_custom: bool,
    free_space: Option<u64>,
    analysis: Option<Analysis>,
    mode: Mode,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cancel: CancelFlag,
    cancelling: bool,
    snap: Option<Snapshot>,
    job_started: Option<Instant>,
    report: Option<Report>,
    error: Option<String>,
    confirm_slow: bool,
    log: VecDeque<String>,
    app_started: Instant,
    limits: Limits,
    resources: (u64, usize),
    log_path: Option<PathBuf>,
}

impl MegaApp {
    pub fn new(cc: &eframe::CreationContext<'_>, log_path: Option<PathBuf>) -> Self {
        theme::apply(&cc.egui_ctx);
        let (tx, rx) = unbounded();
        let mut app = Self {
            machine: Machine::default(),
            path: None,
            dest: None,
            dest_custom: false,
            free_space: None,
            analysis: None,
            mode: Mode::Fast,
            tx,
            rx,
            cancel: CancelFlag::new(),
            cancelling: false,
            snap: None,
            job_started: None,
            report: None,
            error: None,
            confirm_slow: false,
            log: VecDeque::new(),
            app_started: Instant::now(),
            limits: Limits::from_env(),
            // RAM y núcleos se consultan UNA vez (no por frame).
            resources: system_resources(),
            log_path,
        };
        app.push_log("MegaCompress listo. Selecciona o arrastra un archivo ZIP.");
        app
    }

    fn repaint_cb(ctx: &egui::Context) -> Repaint {
        let ctx = ctx.clone();
        Arc::new(move || ctx.request_repaint())
    }

    fn push_log(&mut self, text: impl Into<String>) {
        if self.log.len() >= LOG_MAX_LINES {
            self.log.pop_front();
        }
        self.log
            .push_back(format_log_line(self.app_started.elapsed(), &text.into()));
    }

    fn config(&self, mode: Mode) -> Config {
        Config::for_system(mode, self.resources.0, self.resources.1)
    }

    fn reset_results(&mut self) {
        self.analysis = None;
        self.snap = None;
        self.report = None;
        self.error = None;
        self.confirm_slow = false;
        self.cancelling = false;
        self.job_started = None;
        // Canal nuevo: descarta mensajes rezagados de un trabajo anterior.
        let (tx, rx) = unbounded();
        self.tx = tx;
        self.rx = rx;
    }

    fn refresh_free_space(&mut self, ctx: &egui::Context) {
        if let Some(d) = self.dest.clone() {
            orchestrator::spawn_free_space(d, self.tx.clone(), Some(Self::repaint_cb(ctx)));
        }
    }

    fn select_file(&mut self, ctx: &egui::Context, path: PathBuf) {
        if !self.machine.can_select() {
            return;
        }
        self.reset_results();
        self.machine.go(Stage::FileSelected);
        if !self.dest_custom {
            self.dest = path.parent().map(|p| p.to_path_buf());
        }
        self.free_space = None;
        self.push_log(format!("Archivo seleccionado: {}", path.display()));
        self.push_log("Validando el ZIP…");
        self.path = Some(path.clone());
        self.machine.go(Stage::Validating);
        let dest = self.dest.clone().unwrap_or_else(|| PathBuf::from("."));
        orchestrator::spawn_analyze(
            path,
            dest,
            self.limits.clone(),
            self.tx.clone(),
            Some(Self::repaint_cb(ctx)),
        );
        self.refresh_free_space(ctx);
    }

    fn pick_zip(&mut self, ctx: &egui::Context) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Archivo ZIP", &["zip"])
            .pick_file()
        {
            self.select_file(ctx, p);
        }
    }

    fn pick_dest(&mut self, ctx: &egui::Context) {
        if let Some(d) = rfd::FileDialog::new().pick_folder() {
            self.push_log(format!("Carpeta de destino: {}", d.display()));
            self.dest = Some(d);
            self.dest_custom = true;
            self.free_space = None;
            self.refresh_free_space(ctx);
        }
    }

    fn start_job(&mut self, ctx: &egui::Context) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if !self.machine.go(Stage::Compressing) {
            return;
        }
        self.confirm_slow = false;
        self.cancel = CancelFlag::new();
        self.cancelling = false;
        self.snap = None;
        self.error = None;
        self.job_started = Some(Instant::now());
        let mut job = Job::new(path, self.mode);
        job.out_dir = self.dest.clone();
        job.limits = self.limits.clone();
        self.push_log("Iniciando compresión…");
        orchestrator::spawn(
            job,
            self.cancel.clone(),
            self.tx.clone(),
            Some(Self::repaint_cb(ctx)),
        );
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(ctx, msg);
        }
    }

    fn fail(&mut self, e: &Error) {
        self.error = Some(e.to_string());
        self.push_log(format!("Error: {e}"));
        self.machine.go(Stage::Error);
    }

    fn handle(&mut self, ctx: &egui::Context, msg: Msg) {
        match msg {
            Msg::Analysis(Ok(a)) => {
                if a.mostly_compressed {
                    self.mode = Mode::Fast;
                }
                self.push_log(format!(
                    "ZIP válido: {} archivos, {} sin comprimir.",
                    a.info.files,
                    human_size(a.info.total_bytes)
                ));
                if a.cleaned_orphans > 0 {
                    self.push_log(format!(
                        "Se eliminaron {} temporales abandonados.",
                        a.cleaned_orphans
                    ));
                }
                self.analysis = Some(a);
                self.machine.go(Stage::Ready);
            }
            Msg::Analysis(Err(e)) => self.fail(&e),
            Msg::FreeSpace { dir, bytes } => {
                if self.dest.as_ref() == Some(&dir) {
                    self.free_space = bytes;
                }
            }
            Msg::Log(t) => self.push_log(t),
            Msg::Progress(s) => {
                let active = matches!(self.machine.stage(), Stage::Compressing | Stage::Finishing);
                if active {
                    if s.phase != Phase::Compressing && self.machine.stage() == Stage::Compressing {
                        self.machine.go(Stage::Finishing);
                    }
                    if !s.status.is_empty()
                        && self.snap.as_ref().is_none_or(|o| o.status != s.status)
                    {
                        let status = s.status.clone();
                        self.push_log(status);
                    }
                    self.snap = Some(s);
                }
            }
            Msg::Finished(r) => {
                match r {
                    Ok(rep) => {
                        if self.machine.stage() == Stage::Compressing {
                            self.machine.go(Stage::Finishing);
                        }
                        self.report = Some(rep);
                        self.machine.go(Stage::Done);
                    }
                    Err(Error::Cancelled) => {
                        self.push_log("Operación cancelada. Temporales eliminados; el original no se modificó.");
                        self.machine.go(Stage::Cancelled);
                    }
                    Err(e) => self.fail(&e),
                }
                self.cancelling = false;
                self.refresh_free_space(ctx);
            }
        }
    }

    // ───────────────────────── dibujo ─────────────────────────

    fn sidebar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, hovering: bool) {
        let stage = self.machine.stage();
        let busy = matches!(
            stage,
            Stage::Compressing | Stage::Finishing | Stage::Validating | Stage::FileSelected
        );

        ui.label(
            RichText::new("MegaCompress")
                .size(22.0)
                .strong()
                .color(theme::TEXT),
        );
        ui.label(widgets::dim(
            "Comprime archivos grandes de forma inteligente",
        ));
        ui.add_space(6.0);

        widgets::section_label(ui, "Archivo de origen");
        widgets::flat_box(ui, |ui| {
            if hovering {
                ui.colored_label(theme::ACCENT, "Suelta aquí el archivo ZIP");
            }
            match &self.path {
                Some(p) => {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.add(egui::Label::new(RichText::new(name).strong()).truncate());
                    ui.add(egui::Label::new(widgets::dim(p.display().to_string())).truncate());
                    match stage {
                        Stage::Validating | Stage::FileSelected => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(widgets::dim("Validando…"));
                            });
                        }
                        Stage::Error if self.analysis.is_none() => {
                            ui.colored_label(theme::ERR, "No válido");
                        }
                        _ if self.analysis.is_some() => {
                            ui.colored_label(theme::OK, "ZIP válido");
                        }
                        _ => {}
                    }
                }
                None => {
                    ui.label(widgets::dim(
                        "Ningún archivo seleccionado. Arrastra un ZIP a esta ventana.",
                    ));
                }
            }
        });
        if ui
            .add_enabled(!busy, egui::Button::new("Examinar…"))
            .clicked()
        {
            self.pick_zip(ctx);
        }

        widgets::section_label(ui, "Guardar en");
        widgets::flat_box(ui, |ui| match &self.dest {
            Some(d) => {
                ui.add(egui::Label::new(widgets::dim(d.display().to_string())).truncate());
            }
            None => {
                ui.label(widgets::dim("Se usará la carpeta del ZIP"));
            }
        });
        if ui
            .add_enabled(!busy, egui::Button::new("Elegir carpeta…"))
            .clicked()
        {
            self.pick_dest(ctx);
        }

        widgets::section_label(ui, "Compresión");
        let can_pick_mode = matches!(
            stage,
            Stage::Idle | Stage::Ready | Stage::Done | Stage::Cancelled | Stage::Error
        );
        if widgets::option_box(
            ui,
            "mode_fast",
            "Rápido (Zstandard)",
            "Genera .tar.zst. Muy veloz y multihilo.",
            self.mode == Mode::Fast,
            can_pick_mode,
        )
        .clicked()
            && can_pick_mode
        {
            self.mode = Mode::Fast;
            self.confirm_slow = false;
        }
        if widgets::option_box(
            ui,
            "mode_max",
            "Máxima (7z LZMA2)",
            "Genera .7z sólido. Menor tamaño, más lento.",
            self.mode == Mode::Max,
            can_pick_mode,
        )
        .clicked()
            && can_pick_mode
        {
            self.mode = Mode::Max;
        }
        ui.label(widgets::dim("El archivo original nunca se modifica."));
        ui.add_space(8.0);

        let infeasible = self
            .space_plan()
            .is_some_and(|p| p.verdict == Verdict::Insufficient);
        let ready = stage == Stage::Ready && self.path.is_some() && !infeasible;
        let working = matches!(stage, Stage::Compressing | Stage::Finishing);

        if self.confirm_slow {
            let est = self
                .analysis
                .as_ref()
                .and_then(|a| a.est_max)
                .unwrap_or(SLOW_WARNING);
            widgets::flat_box(ui, |ui| {
                ui.colored_label(
                    theme::WARN,
                    format!(
                        "Máxima podría tardar unas {} (estimación). ¿Continuar?",
                        human_duration(est)
                    ),
                );
                ui.horizontal(|ui| {
                    if ui.button("Continuar").clicked() {
                        self.start_job(ctx);
                    }
                    if ui.button("Usar Rápido").clicked() {
                        self.mode = Mode::Fast;
                        self.confirm_slow = false;
                    }
                });
            });
        } else if !working && widgets::primary_button(ui, "COMPRIMIR", ready).clicked() {
            let slow = self.mode == Mode::Max
                && self
                    .analysis
                    .as_ref()
                    .and_then(|a| a.est_max)
                    .is_some_and(|d| d > SLOW_WARNING);
            if slow {
                self.confirm_slow = true;
            } else {
                self.start_job(ctx);
            }
        }

        if working {
            if self.cancelling {
                ui.colored_label(
                    theme::WARN,
                    if self.mode == Mode::Max {
                        "Cancelando… esperando a que termine el bloque sólido en curso (no es interrumpible)."
                    } else {
                        "Cancelando…"
                    },
                );
            } else if widgets::full_width_secondary(ui, "CANCELAR").clicked() {
                self.cancel.cancel();
                self.cancelling = true;
                self.push_log("Cancelación solicitada.");
            }
        }
        if stage == Stage::Cancelled || stage == Stage::Error {
            if let Some(p) = self.path.clone() {
                if widgets::full_width_secondary(ui, "Volver a validar").clicked() {
                    self.select_file(ctx, p);
                }
            }
        }
        if let Some(lp) = &self.log_path {
            ui.add_space(8.0);
            ui.add(
                egui::Label::new(widgets::dim(format!("Log técnico: {}", lp.display()))).truncate(),
            );
        }
    }

    fn space_plan(&self) -> Option<megacompress::fs::space::SpacePlan> {
        let a = self.analysis.as_ref()?;
        Some(plan(&a.info, self.mode, self.free_space))
    }

    fn stats_grid(&self, ui: &mut egui::Ui) {
        let cfg = self.config(self.mode);
        let threads = match self.mode {
            Mode::Fast => cfg.zstd_workers,
            Mode::Max => cfg.lzma_threads,
        };
        let na = "No disponible".to_string();
        let original = self
            .analysis
            .as_ref()
            .map(|a| human_size(a.info.zip_size))
            .unwrap_or_else(|| na.clone());
        let estimated = self
            .analysis
            .as_ref()
            .and_then(|a| match self.mode {
                Mode::Fast => a.est_size_fast,
                Mode::Max => a.est_size_max,
            })
            .map(|b| format!("≈ {} (estimación)", human_size(b)))
            .unwrap_or_else(|| na.clone());
        let free = self
            .free_space
            .map(human_size)
            .unwrap_or_else(|| na.clone());
        let ram = format!("≈ {} (estimación)", human_size(cfg.estimated_ram()));
        let fmt = format!(".{}", self.mode.extension());

        let items: [(&str, String); 6] = [
            ("Tamaño original", original),
            ("Tamaño estimado", estimated),
            ("Espacio libre", free),
            ("Hilos utilizados", threads.to_string()),
            ("RAM estimada", ram),
            ("Formato de salida", fmt),
        ];
        // Responsivo: 3 columnas si hay sitio, si no 2 o 1. El ancho de cada columna se fija
        // explícitamente: sin esto, `Grid` puede dar a una celda un ancho casi nulo y el texto
        // se parte letra por letra ("Tamañ o origina l").
        let w = ui.available_width();
        let spacing = 24.0_f32;
        let cols = if w >= 560.0 {
            3
        } else if w >= 360.0 {
            2
        } else {
            1
        };
        let cell_w = ((w - spacing * (cols as f32 - 1.0)) / cols as f32).max(110.0);
        egui::Grid::new("stats")
            .num_columns(cols)
            .min_col_width(cell_w)
            .max_col_width(cell_w)
            .spacing([spacing, 14.0])
            .show(ui, |ui| {
                for (i, (label, value)) in items.iter().enumerate() {
                    let color = if value == "No disponible" {
                        theme::TEXT_DIM
                    } else {
                        theme::TEXT
                    };
                    widgets::stat(ui, label, value, color);
                    if (i + 1) % cols == 0 {
                        ui.end_row();
                    }
                }
            });
    }

    fn warnings(&self, ui: &mut egui::Ui) {
        let Some(a) = &self.analysis else { return };
        let est = match self.mode {
            Mode::Fast => a.est_size_fast,
            Mode::Max => a.est_size_max,
        };
        let may_grow = est.map_or(a.mostly_compressed, |e| e > a.info.zip_size);
        if may_grow {
            ui.colored_label(
                theme::WARN,
                "Aviso: el archivo final podría ser más grande que el original porque los archivos ya están comprimidos.",
            );
        }
        if let Some(p) = self.space_plan() {
            match &p.verdict {
                Verdict::Warn(m) => {
                    ui.colored_label(theme::WARN, m.clone());
                }
                Verdict::Insufficient => {
                    ui.colored_label(
                        theme::ERR,
                        format!(
                            "Espacio insuficiente en el destino: se necesitan al menos {} y hay {} libres.",
                            human_size(p.floor),
                            human_size(p.available.unwrap_or(0))
                        ),
                    );
                }
                Verdict::Ok => {}
            }
        }
        if a.info.symlinks > 0 {
            ui.colored_label(
                theme::WARN,
                format!(
                    "{} enlaces simbólicos del ZIP no se incluirán en el resultado.",
                    a.info.symlinks
                ),
            );
        }
    }

    fn state_text(&self) -> (&'static str, Color32) {
        match self.machine.stage() {
            Stage::Idle => ("En espera", theme::TEXT_DIM),
            Stage::FileSelected | Stage::Validating => ("Validando", theme::TEXT_DIM),
            Stage::Ready => ("Listo para comprimir", theme::TEXT),
            Stage::Compressing => ("Comprimiendo", theme::ACCENT),
            Stage::Finishing => match self.snap.as_ref().map(|s| s.phase) {
                Some(Phase::Verifying) => ("Verificando", theme::ACCENT),
                _ => ("Finalizando", theme::ACCENT),
            },
            Stage::Done => ("Completado", theme::OK),
            Stage::Cancelled => ("Cancelado", theme::WARN),
            Stage::Error => ("Error", theme::ERR),
        }
    }

    fn progress_block(&self, ui: &mut egui::Ui) {
        let stage = self.machine.stage();
        let (state, state_color) = self.state_text();
        let fraction = match stage {
            Stage::Done => 1.0,
            Stage::Compressing | Stage::Finishing => {
                self.snap.as_ref().map_or(0.0, |s| s.overall_fraction())
            }
            _ => 0.0,
        };
        let bar_color = match stage {
            Stage::Done => theme::OK,
            Stage::Error => theme::ERR,
            Stage::Cancelled => theme::WARN,
            _ => theme::ACCENT,
        };
        // `ProgressBar` pinta la pista con `extreme_bg_color` (theme::TRACK). A 0 % egui dibuja
        // igualmente un punto del color de relleno; se usa el color de la pista para que no
        // aparezca ese punto suelto y la barra se vea como una pista completa.
        let fill = if fraction > 0.0 {
            bar_color
        } else {
            theme::TRACK
        };
        ui.add(
            egui::ProgressBar::new(fraction)
                .desired_width(ui.available_width())
                .desired_height(14.0)
                .corner_radius(4)
                .fill(fill),
        );
        let phase_name = match (stage, self.snap.as_ref()) {
            (Stage::Done, _) => "Operación completada",
            (Stage::Compressing | Stage::Finishing, Some(s)) => s.phase.label(),
            (Stage::Compressing, None) => "Preparando",
            _ => "—",
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{:.1} %", fraction * 100.0)).strong());
            ui.label(widgets::dim("·"));
            ui.label(phase_name);
            ui.label(widgets::dim("·"));
            ui.label(RichText::new(state).color(state_color).strong());
        });

        let active = matches!(stage, Stage::Compressing | Stage::Finishing);
        if let Some(s) = self.snap.as_ref().filter(|_| active) {
            let elapsed = self.job_started.map(|t| t.elapsed()).unwrap_or_default();
            let eta = match s.eta {
                Some(d) if s.phase != Phase::Finalizing => human_duration(d),
                _ if s.phase == Phase::Finalizing => "—".into(),
                _ => "Calculando…".into(),
            };
            let speed = if s.phase == Phase::Finalizing {
                "—".into()
            } else {
                human_rate(s.speed_bps)
            };
            ui.label(widgets::dim(format!(
                "Velocidad {speed} · Transcurrido {} · ETA {eta}",
                human_duration(elapsed)
            )));
            match s.phase {
                Phase::Compressing => {
                    ui.label(format!(
                        "Archivos {} / {} · {} de {}",
                        s.entries_done,
                        s.entries_total,
                        human_size(s.done),
                        human_size(s.total)
                    ));
                }
                Phase::Finalizing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(if s.status.is_empty() {
                            "Finalizando…"
                        } else {
                            s.status.as_str()
                        });
                    });
                }
                Phase::Verifying => {
                    ui.label(format!(
                        "Entradas verificadas {} · pendientes {} · bytes verificados {} de {}",
                        s.entries_done,
                        s.entries_total.saturating_sub(s.entries_done),
                        human_size(s.done),
                        human_size(s.total)
                    ));
                    ui.label(widgets::dim(
                        "Comprobando CRC32 del contenido recuperado contra el ZIP original.",
                    ));
                }
            }
        }
        if let Some(e) = &self.error {
            ui.colored_label(theme::ERR, e.clone());
        }
    }

    fn result_block(&mut self, ui: &mut egui::Ui) {
        let Some(rep) = self.report.clone() else {
            return;
        };
        ui.separator();
        ui.label(RichText::new("Resultado").strong());
        egui::Grid::new("resultado")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                let row = |ui: &mut egui::Ui, k: &str, v: String| {
                    ui.label(widgets::dim(k));
                    ui.label(v);
                    ui.end_row();
                };
                row(ui, "Original", human_size(rep.original_size));
                row(ui, "Final", human_size(rep.output_size));
                let diff = i128::from(rep.original_size) - i128::from(rep.output_size);
                let pct = if rep.original_size > 0 {
                    diff as f64 / rep.original_size as f64 * 100.0
                } else {
                    0.0
                };
                if diff >= 0 {
                    row(
                        ui,
                        "Ahorro",
                        format!("{} ({pct:.1} %)", human_size(diff as u64)),
                    );
                } else {
                    row(
                        ui,
                        "Resultado",
                        format!("{} MÁS grande ({:.1} %)", human_size((-diff) as u64), -pct),
                    );
                }
                row(ui, "Tiempo", human_duration(rep.elapsed));
                row(ui, "Formato", rep.format.name().to_string());
                row(ui, "Ruta", rep.path.display().to_string());
            });
        ui.label(widgets::dim(rep.verification.clone()));
        if rep.skipped_symlinks > 0 {
            ui.colored_label(
                theme::WARN,
                format!(
                    "{} enlaces simbólicos no se incluyeron.",
                    rep.skipped_symlinks
                ),
            );
        }
        ui.horizontal(|ui| {
            if widgets::secondary_button(ui, "Abrir carpeta", true).clicked() {
                if let Some(dir) = rep.path.parent() {
                    let _ = open::that(dir);
                }
            }
            if widgets::secondary_button(ui, "Comprimir otro archivo", true).clicked() {
                self.path = None;
                self.reset_results();
                self.machine.go(Stage::Idle);
            }
        });
    }

    fn console(&self, ui: &mut egui::Ui) {
        ui.label(widgets::dim("Registro"));
        let height = ui.available_height().max(120.0);
        widgets::flat_box(ui, |ui| {
            let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
            egui::ScrollArea::both()
                .max_height(height - 24.0)
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show_rows(ui, row_h, self.log.len(), |ui, range| {
                    for line in self
                        .log
                        .iter()
                        .skip(range.start)
                        .take(range.end - range.start)
                    {
                        // Sin ajuste de línea: el corchete de cierre del tiempo nunca se queda
                        // en una línea distinta de la marca de tiempo.
                        ui.add(
                            egui::Label::new(RichText::new(line).monospace().size(12.0))
                                .wrap_mode(egui::TextWrapMode::Extend)
                                .selectable(true),
                        );
                    }
                });
        });
    }
}

impl eframe::App for MegaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Arrastrar y soltar (egui::RawInput::dropped_files).
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(p) = dropped.into_iter().find_map(|f| f.path) {
            self.select_file(ctx, p);
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        self.poll(ctx);

        // Mientras hay trabajo activo, refrescar 2 veces/s para el cronómetro (en reposo, nada).
        if matches!(self.machine.stage(), Stage::Compressing | Stage::Finishing) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        let side_w = (ctx.screen_rect().width() * 0.42).clamp(240.0, 300.0);
        egui::SidePanel::left("sidebar")
            .exact_width(side_w)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.sidebar(ui, ctx, hovering);
                    });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                self.stats_grid(ui);
                ui.add_space(4.0);
                self.warnings(ui);
                ui.separator();
                self.progress_block(ui);
                self.result_block(ui);
                ui.add_space(6.0);
                self.console(ui);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_line_has_both_brackets_and_a_space() {
        let s = format_log_line(Duration::from_millis(100), "hola");
        assert_eq!(s, "[    0.1s] hola");
        assert!(s.starts_with('['));
        assert!(s.contains("s] "));
    }

    #[test]
    fn log_line_does_not_depend_on_text_width() {
        for secs in [0.0_f64, 0.1, 9.9, 123.4, 12_345.6] {
            let s = format_log_line(Duration::from_secs_f64(secs), "mensaje");
            assert!(s.starts_with('['), "{s}");
            assert!(s.contains("s] mensaje"), "{s}");
        }
    }
}
