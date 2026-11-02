//! Progreso en tres etapas reales:
//!
//! | Fase | Rango visual | Qué mide |
//! |---|---|---|
//! | `Compressing` | 0 % – 80 %  | bytes DESCOMPRIMIDOS entregados al Sink ÷ total |
//! | `Finalizing`  | 80 % – 90 % | sin porcentaje interno (el motor no lo da): se muestra el paso real |
//! | `Verifying`   | 90 % – 100 %| bytes verificados ÷ total esperado; nunca llega a 100 % |
//!
//! El 100 % solo lo muestra la interfaz al recibir `Msg::Finished(Ok)`, es decir, con el contenedor
//! finalizado, la verificación terminada sin discrepancias y el resultado ya publicado.
//!
//! Eventos como máximo 20/s (throttle), velocidad con ventana móvil de 2 s y ETA solo tras ≥ 2 s
//! de medición real.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const MAX_EVENTS_PER_SEC: u32 = 20;
pub const SPEED_WINDOW: Duration = Duration::from_secs(2);
pub const ETA_MIN_MEASURE: Duration = Duration::from_secs(2);

/// Límites visuales de cada fase (fracción 0..1 de la barra global).
pub const COMPRESS_END: f32 = 0.80;
pub const FINALIZE_END: f32 = 0.90;
/// Tope mientras se verifica: 100 % solo tras publicar.
pub const VERIFY_CAP: f32 = 0.995;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Compressing,
    Finalizing,
    Verifying,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Compressing => "Lectura y compresión",
            Phase::Finalizing => "Finalización del contenedor",
            Phase::Verifying => "Verificación de integridad",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub phase: Phase,
    /// Bytes procesados en la fase actual (comprimidos o verificados).
    pub done: u64,
    pub total: u64,
    pub entries_done: u64,
    pub entries_total: u64,
    pub speed_bps: f64,
    /// `None` = "Calculando…" (aún no hay 2 s de medición o velocidad nula).
    pub eta: Option<Duration>,
    /// Tiempo transcurrido de la FASE.
    pub elapsed: Duration,
    /// Paso real en curso (p. ej. "Cerrando frame Zstandard").
    pub status: String,
    /// Entrada que se está procesando.
    pub current: Option<String>,
}

impl Snapshot {
    /// Fracción 0..1 de la fase actual.
    pub fn phase_fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }

    /// Fracción 0..1 de la barra global (ver tabla del módulo).
    pub fn overall_fraction(&self) -> f32 {
        match self.phase {
            Phase::Compressing => COMPRESS_END * self.phase_fraction(),
            Phase::Finalizing => COMPRESS_END,
            Phase::Verifying => {
                (FINALIZE_END + (1.0 - FINALIZE_END) * self.phase_fraction()).min(VERIFY_CAP)
            }
        }
    }

    pub fn indeterminate(&self) -> bool {
        self.phase == Phase::Finalizing
    }
}

pub struct Throttle {
    min_interval: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub fn per_second(n: u32) -> Self {
        Self {
            min_interval: Duration::from_secs_f64(1.0 / n.max(1) as f64),
            last: None,
        }
    }

    /// ¿Se puede emitir un evento en `now`? Si sí, lo registra.
    pub fn ready(&mut self, now: Instant) -> bool {
        match self.last {
            Some(l) if now.saturating_duration_since(l) < self.min_interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

pub struct SpeedWindow {
    window: Duration,
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedWindow {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::new(),
        }
    }

    pub fn push(&mut self, now: Instant, done: u64) {
        self.samples.push_back((now, done));
        while self.samples.len() > 2 {
            match self.samples.front() {
                Some(&(t, _)) if now.saturating_duration_since(t) > self.window => {
                    self.samples.pop_front();
                }
                _ => break,
            }
        }
    }

    pub fn speed(&self) -> f64 {
        match (self.samples.front(), self.samples.back()) {
            (Some(&(t0, d0)), Some(&(t1, d1))) => {
                let dt = t1.saturating_duration_since(t0).as_secs_f64();
                if dt <= 0.0 {
                    0.0
                } else {
                    d1.saturating_sub(d0) as f64 / dt
                }
            }
            _ => 0.0,
        }
    }
}

pub struct Tracker {
    phase: Phase,
    total: u64,
    done: u64,
    entries_total: u64,
    entries_done: u64,
    started: Instant,
    throttle: Throttle,
    window: SpeedWindow,
    status: String,
    current: Option<String>,
    emit: Box<dyn FnMut(Snapshot) + Send>,
}

impl Tracker {
    /// Tracker de la fase de compresión.
    pub fn new(total: u64, entries_total: u64, emit: Box<dyn FnMut(Snapshot) + Send>) -> Self {
        Self::new_at(
            Instant::now(),
            Phase::Compressing,
            total,
            entries_total,
            emit,
        )
    }

    pub fn for_phase(
        phase: Phase,
        total: u64,
        entries_total: u64,
        emit: Box<dyn FnMut(Snapshot) + Send>,
    ) -> Self {
        Self::new_at(Instant::now(), phase, total, entries_total, emit)
    }

    pub fn new_at(
        started: Instant,
        phase: Phase,
        total: u64,
        entries_total: u64,
        emit: Box<dyn FnMut(Snapshot) + Send>,
    ) -> Self {
        Self {
            phase,
            total,
            done: 0,
            entries_total,
            entries_done: 0,
            started,
            throttle: Throttle::per_second(MAX_EVENTS_PER_SEC),
            window: SpeedWindow::new(SPEED_WINDOW),
            status: String::new(),
            current: None,
            emit,
        }
    }

    pub fn begin_entry(&mut self, name: &str) {
        self.current = Some(name.to_string());
    }

    pub fn add_bytes(&mut self, n: u64) {
        self.add_bytes_at(n, Instant::now());
    }

    pub fn add_bytes_at(&mut self, n: u64, now: Instant) {
        self.done += n;
        if self.throttle.ready(now) {
            self.emit_at(now);
        }
    }

    /// Fija valores absolutos (la verificación informa del total acumulado, no de incrementos).
    pub fn set_absolute(&mut self, done: u64, entries_done: u64) {
        self.set_absolute_at(done, entries_done, Instant::now());
    }

    pub fn set_absolute_at(&mut self, done: u64, entries_done: u64, now: Instant) {
        self.done = done;
        self.entries_done = entries_done;
        if self.throttle.ready(now) {
            self.emit_at(now);
        }
    }

    pub fn entry_done(&mut self) {
        self.entries_done += 1;
    }

    /// Emite un evento sin respetar el throttle.
    pub fn flush(&mut self) {
        self.emit_at(Instant::now());
    }

    fn emit_at(&mut self, now: Instant) {
        self.window.push(now, self.done);
        let speed = self.window.speed();
        let elapsed = now.saturating_duration_since(self.started);
        let eta = if elapsed >= ETA_MIN_MEASURE && speed > 0.0 {
            let remaining = self.total.saturating_sub(self.done) as f64;
            Some(Duration::from_secs_f64(remaining / speed))
        } else {
            None
        };
        (self.emit)(Snapshot {
            phase: self.phase,
            done: self.done,
            total: self.total,
            entries_done: self.entries_done,
            entries_total: self.entries_total,
            speed_bps: speed,
            eta,
            elapsed,
            status: self.status.clone(),
            current: self.current.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[allow(clippy::type_complexity)]
    fn collector() -> (Arc<Mutex<Vec<Snapshot>>>, Box<dyn FnMut(Snapshot) + Send>) {
        let store = Arc::new(Mutex::new(Vec::new()));
        let s2 = store.clone();
        (store, Box::new(move |s| s2.lock().unwrap().push(s)))
    }

    fn snap(phase: Phase, done: u64, total: u64) -> Snapshot {
        Snapshot {
            phase,
            done,
            total,
            entries_done: 0,
            entries_total: 0,
            speed_bps: 0.0,
            eta: None,
            elapsed: Duration::ZERO,
            status: String::new(),
            current: None,
        }
    }

    #[test]
    fn throttle_caps_at_20_events_per_second() {
        let base = Instant::now();
        let (store, emit) = collector();
        let mut t = Tracker::new_at(base, Phase::Compressing, 1_000_000, 1, emit);
        for i in 0..10_000u64 {
            t.add_bytes_at(1, base + Duration::from_micros(i * 100));
        }
        let n = store.lock().unwrap().len();
        assert!(n <= 20, "se emitieron {n} eventos en 1 s");
        assert!(n >= 19, "demasiado pocos: {n}");
    }

    #[test]
    fn no_eta_before_two_seconds_then_eta() {
        let base = Instant::now();
        let (store, emit) = collector();
        let mut t = Tracker::new_at(base, Phase::Compressing, 10_000, 1, emit);
        t.add_bytes_at(1_000, base + Duration::from_millis(100));
        t.add_bytes_at(1_000, base + Duration::from_millis(1_900));
        assert!(store.lock().unwrap().iter().all(|s| s.eta.is_none()));
        t.add_bytes_at(1_000, base + Duration::from_millis(2_500));
        t.add_bytes_at(1_000, base + Duration::from_millis(3_000));
        let last = store.lock().unwrap().last().cloned().unwrap();
        assert!(last.eta.is_some());
        assert!(last.speed_bps > 0.0);
    }

    #[test]
    fn speed_uses_moving_window() {
        let base = Instant::now();
        let mut w = SpeedWindow::new(Duration::from_secs(2));
        w.push(base, 0);
        w.push(base + Duration::from_secs(10), 1_000_000);
        w.push(base + Duration::from_secs(11), 2_000_000);
        w.push(base + Duration::from_secs(12), 3_000_000);
        let v = w.speed();
        assert!((v - 1_000_000.0).abs() < 1.0, "{v}");
    }

    #[test]
    fn overall_fraction_follows_the_three_stages() {
        assert_eq!(snap(Phase::Compressing, 0, 100).overall_fraction(), 0.0);
        assert!((snap(Phase::Compressing, 50, 100).overall_fraction() - 0.40).abs() < 1e-6);
        assert!(
            (snap(Phase::Compressing, 100, 100).overall_fraction() - COMPRESS_END).abs() < 1e-6
        );
        assert_eq!(
            snap(Phase::Finalizing, 0, 0).overall_fraction(),
            COMPRESS_END
        );
        assert!(snap(Phase::Finalizing, 0, 0).indeterminate());
        assert!((snap(Phase::Verifying, 0, 100).overall_fraction() - FINALIZE_END).abs() < 1e-6);
        assert!(snap(Phase::Verifying, 50, 100).overall_fraction() > FINALIZE_END);
    }

    #[test]
    fn never_reaches_100_percent_while_verifying() {
        for done in [0u64, 50, 99, 100, 5000] {
            let f = snap(Phase::Verifying, done, 100).overall_fraction();
            assert!(f < 1.0, "verificando no puede mostrar 100 %: {f}");
        }
        // Tampoco con total = 0 (nada que verificar todavía).
        assert!(snap(Phase::Verifying, 0, 0).overall_fraction() < 1.0);
    }

    #[test]
    fn set_absolute_reports_totals() {
        let base = Instant::now();
        let (store, emit) = collector();
        let mut t = Tracker::new_at(base, Phase::Verifying, 1000, 4, emit);
        t.set_absolute_at(300, 1, base);
        let s = store.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            (s.done, s.entries_done, s.phase),
            (300, 1, Phase::Verifying)
        );
    }
}
