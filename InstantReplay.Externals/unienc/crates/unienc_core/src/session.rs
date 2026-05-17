use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Recording,
    WaitForRecordingComplete,
    Exporting,
    Completed,
    Invalid,
}

/// Manages the lifecycle state of a replay session.
///
/// Transitions:
///   Recording → WaitForRecordingComplete → Exporting → Completed
/// On error: any state → Invalid
pub struct SessionController {
    state: Mutex<SessionState>,
}

impl SessionController {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SessionState::Recording),
        }
    }

    pub fn state(&self) -> SessionState {
        *self.state.lock().unwrap()
    }

    /// Transition Recording → WaitForRecordingComplete.
    /// Returns `Err` if the current state is not `Recording`.
    pub fn begin_stop(&self) -> Result<(), SessionStateError> {
        let mut s = self.state.lock().unwrap();
        if *s != SessionState::Recording {
            return Err(SessionStateError::InvalidTransition {
                from: *s,
                to: SessionState::WaitForRecordingComplete,
            });
        }
        *s = SessionState::WaitForRecordingComplete;
        Ok(())
    }

    /// Transition WaitForRecordingComplete → Exporting.
    pub fn begin_export(&self) -> Result<(), SessionStateError> {
        let mut s = self.state.lock().unwrap();
        if *s != SessionState::WaitForRecordingComplete {
            return Err(SessionStateError::InvalidTransition {
                from: *s,
                to: SessionState::Exporting,
            });
        }
        *s = SessionState::Exporting;
        Ok(())
    }

    /// Transition Exporting → Completed.
    pub fn complete(&self) -> Result<(), SessionStateError> {
        let mut s = self.state.lock().unwrap();
        if *s != SessionState::Exporting {
            return Err(SessionStateError::InvalidTransition {
                from: *s,
                to: SessionState::Completed,
            });
        }
        *s = SessionState::Completed;
        Ok(())
    }

    /// Transition any state → Invalid.
    pub fn fail(&self) {
        *self.state.lock().unwrap() = SessionState::Invalid;
    }
}

impl Default for SessionController {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum SessionStateError {
    InvalidTransition {
        from: SessionState,
        to: SessionState,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_in_recording_state() {
        let sc = SessionController::new();
        assert_eq!(sc.state(), SessionState::Recording);
    }

    #[test]
    fn happy_path_transition_sequence() {
        let sc = SessionController::new();
        sc.begin_stop().unwrap();
        assert_eq!(sc.state(), SessionState::WaitForRecordingComplete);
        sc.begin_export().unwrap();
        assert_eq!(sc.state(), SessionState::Exporting);
        sc.complete().unwrap();
        assert_eq!(sc.state(), SessionState::Completed);
    }

    #[test]
    fn begin_stop_twice_returns_error() {
        let sc = SessionController::new();
        sc.begin_stop().unwrap();
        assert!(sc.begin_stop().is_err());
    }

    #[test]
    fn begin_export_without_stop_returns_error() {
        let sc = SessionController::new();
        assert!(sc.begin_export().is_err());
    }

    #[test]
    fn fail_moves_to_invalid() {
        let sc = SessionController::new();
        sc.fail();
        assert_eq!(sc.state(), SessionState::Invalid);
    }

    #[test]
    fn cannot_export_after_invalid() {
        let sc = SessionController::new();
        sc.begin_stop().unwrap();
        sc.fail();
        assert!(sc.begin_export().is_err());
    }
}
