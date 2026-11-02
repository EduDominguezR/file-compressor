//! Máquina de estados de la UI. Transiciones explícitas; cualquier otra se rechaza.
//! Idle → FileSelected → Validating → Ready → Compressing → Finishing → Done | Cancelled | Error

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    FileSelected,
    Validating,
    Ready,
    Compressing,
    Finishing,
    Done,
    Cancelled,
    Error,
}

#[derive(Debug)]
pub struct Machine {
    stage: Stage,
}

impl Default for Machine {
    fn default() -> Self {
        Self { stage: Stage::Idle }
    }
}

impl Machine {
    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn allowed(from: Stage, to: Stage) -> bool {
        use Stage::*;
        matches!(
            (from, to),
            (Idle, FileSelected)
                | (FileSelected, Validating)
                | (Validating, Ready)
                | (Validating, Error)
                | (Ready, Compressing)
                | (Ready, FileSelected)
                | (Ready, Idle)
                | (Compressing, Finishing)
                | (Compressing, Cancelled)
                | (Compressing, Error)
                | (Finishing, Done)
                | (Finishing, Cancelled)
                | (Finishing, Error)
                | (Done, FileSelected)
                | (Done, Idle)
                | (Cancelled, FileSelected)
                | (Cancelled, Idle)
                | (Error, FileSelected)
                | (Error, Idle)
        )
    }

    /// Intenta transicionar. Devuelve `false` (y no cambia nada) si la transición no existe.
    pub fn go(&mut self, to: Stage) -> bool {
        if Self::allowed(self.stage, to) {
            self.stage = to;
            true
        } else {
            tracing::warn!(
                "transición de estado rechazada: {:?} -> {:?}",
                self.stage,
                to
            );
            false
        }
    }

    /// ¿Se puede elegir un archivo nuevo ahora? (no durante validación ni compresión)
    pub fn can_select(&self) -> bool {
        matches!(
            self.stage,
            Stage::Idle | Stage::Ready | Stage::Done | Stage::Cancelled | Stage::Error
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path() {
        let mut m = Machine::default();
        for s in [
            Stage::FileSelected,
            Stage::Validating,
            Stage::Ready,
            Stage::Compressing,
            Stage::Finishing,
            Stage::Done,
        ] {
            assert!(m.go(s), "{s:?}");
        }
        assert!(m.can_select());
    }

    #[test]
    fn rejects_invalid() {
        let mut m = Machine::default();
        assert!(!m.go(Stage::Compressing));
        assert_eq!(m.stage(), Stage::Idle);
        assert!(m.go(Stage::FileSelected));
        assert!(!m.go(Stage::Done));
        assert_eq!(m.stage(), Stage::FileSelected);
    }

    #[test]
    fn cannot_select_while_working() {
        let mut m = Machine::default();
        m.go(Stage::FileSelected);
        m.go(Stage::Validating);
        assert!(!m.can_select());
        m.go(Stage::Ready);
        m.go(Stage::Compressing);
        assert!(!m.can_select());
    }
}
