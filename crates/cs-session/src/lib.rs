//! Máquina de estados de una sesión (ver `docs/02-arquitectura.md` §2.7).
//!
//! Es pura: recibe eventos y el instante actual, devuelve acciones que el
//! orquestador (agente) ejecuta. Así las reglas de reconexión y de vida del
//! monitor virtual se prueban sin red ni drivers.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Discovered,
    Pairing,
    Connecting,
    Negotiating,
    Streaming,
    Reconnecting { since: Instant },
    Closed { reason: CloseReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseReason {
    UserStopped,
    PairingFailed,
    GraceExpired,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// El usuario elige conectar; `paired` indica si ya hay huella fijada.
    ConnectRequested {
        paired: bool,
    },
    PairingSucceeded,
    PairingFailed,
    TlsEstablished,
    Negotiated,
    PathLost,
    PathRestored,
    StopRequested,
    Fatal(String),
}

/// Efectos que el orquestador debe ejecutar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    StartPairing,
    OpenConnection,
    SendCapabilities,
    /// Crear monitor virtual (modo extender) e iniciar captura y codificación.
    StartPipeline,
    /// Detener el envío pero **conservar** el monitor virtual.
    PauseEncoder,
    ResumeWithKeyframe,
    StopPipeline,
    DestroyVirtualDisplay,
    CloseConnection,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("transición inválida: {event:?} en estado {state:?}")]
pub struct InvalidTransition {
    pub state: State,
    pub event: Event,
}

#[derive(Debug, Clone)]
pub struct Session {
    state: State,
    grace: Duration,
    /// Si se llegó a crear el pipeline (y por tanto quizá un monitor virtual).
    pipeline_started: bool,
}

impl Session {
    pub const DEFAULT_GRACE: Duration = Duration::from_secs(30);

    pub fn new(grace: Duration) -> Self {
        Session {
            state: State::Discovered,
            grace,
            pipeline_started: false,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn handle(&mut self, event: Event, now: Instant) -> Result<Vec<Action>, InvalidTransition> {
        use Action as A;
        use Event as E;
        use State as S;

        // Eventos válidos en cualquier estado no terminal.
        if !matches!(self.state, S::Closed { .. }) {
            match &event {
                E::StopRequested => return Ok(self.close(CloseReason::UserStopped)),
                E::Fatal(msg) => return Ok(self.close(CloseReason::Error(msg.clone()))),
                _ => {}
            }
        }

        let (next, actions) = match (&self.state, &event) {
            (S::Discovered, E::ConnectRequested { paired: false }) => {
                (S::Pairing, vec![A::StartPairing])
            }
            (S::Discovered, E::ConnectRequested { paired: true }) => {
                (S::Connecting, vec![A::OpenConnection])
            }
            (S::Pairing, E::PairingSucceeded) => (S::Connecting, vec![A::OpenConnection]),
            (S::Pairing, E::PairingFailed) => {
                return Ok(self.close(CloseReason::PairingFailed));
            }
            (S::Connecting, E::TlsEstablished) => (S::Negotiating, vec![A::SendCapabilities]),
            (S::Negotiating, E::Negotiated) => {
                self.pipeline_started = true;
                (S::Streaming, vec![A::StartPipeline])
            }
            (S::Streaming, E::PathLost) => (
                S::Reconnecting { since: now },
                vec![A::PauseEncoder, A::OpenConnection],
            ),
            (S::Reconnecting { .. }, E::PathRestored) => {
                (S::Streaming, vec![A::ResumeWithKeyframe])
            }
            // Un fallo de reconexión individual no cierra: se reintenta hasta la gracia.
            (S::Reconnecting { .. }, E::PathLost) => return Ok(vec![A::OpenConnection]),
            _ => {
                return Err(InvalidTransition {
                    state: self.state.clone(),
                    event,
                })
            }
        };
        self.state = next;
        Ok(actions)
    }

    /// Debe llamarse periódicamente; cierra la sesión si expira la gracia.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        match self.state {
            State::Reconnecting { since } if now.duration_since(since) >= self.grace => {
                self.close(CloseReason::GraceExpired)
            }
            _ => Vec::new(),
        }
    }

    fn close(&mut self, reason: CloseReason) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.pipeline_started {
            actions.push(Action::StopPipeline);
            actions.push(Action::DestroyVirtualDisplay);
            self.pipeline_started = false;
        }
        actions.push(Action::CloseConnection);
        self.state = State::Closed { reason };
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streaming(now: Instant) -> Session {
        let mut s = Session::new(Duration::from_secs(30));
        s.handle(Event::ConnectRequested { paired: true }, now)
            .unwrap();
        s.handle(Event::TlsEstablished, now).unwrap();
        assert_eq!(
            s.handle(Event::Negotiated, now).unwrap(),
            vec![Action::StartPipeline]
        );
        s
    }

    #[test]
    fn unpaired_goes_through_pairing() {
        let now = Instant::now();
        let mut s = Session::new(Session::DEFAULT_GRACE);
        assert_eq!(
            s.handle(Event::ConnectRequested { paired: false }, now)
                .unwrap(),
            vec![Action::StartPairing]
        );
        assert_eq!(
            s.handle(Event::PairingSucceeded, now).unwrap(),
            vec![Action::OpenConnection]
        );
        assert_eq!(s.state(), &State::Connecting);
    }

    #[test]
    fn pairing_failure_closes_without_touching_display() {
        let now = Instant::now();
        let mut s = Session::new(Session::DEFAULT_GRACE);
        s.handle(Event::ConnectRequested { paired: false }, now)
            .unwrap();
        assert_eq!(
            s.handle(Event::PairingFailed, now).unwrap(),
            vec![Action::CloseConnection]
        );
    }

    #[test]
    fn reconnect_within_grace_keeps_display_and_requests_keyframe() {
        let t0 = Instant::now();
        let mut s = streaming(t0);
        assert_eq!(
            s.handle(Event::PathLost, t0).unwrap(),
            vec![Action::PauseEncoder, Action::OpenConnection]
        );
        assert!(s.tick(t0 + Duration::from_secs(29)).is_empty());
        assert_eq!(
            s.handle(Event::PathRestored, t0).unwrap(),
            vec![Action::ResumeWithKeyframe]
        );
        assert_eq!(s.state(), &State::Streaming);
    }

    #[test]
    fn grace_expiry_destroys_virtual_display() {
        let t0 = Instant::now();
        let mut s = streaming(t0);
        s.handle(Event::PathLost, t0).unwrap();
        let actions = s.tick(t0 + Duration::from_secs(30));
        assert_eq!(
            actions,
            vec![
                Action::StopPipeline,
                Action::DestroyVirtualDisplay,
                Action::CloseConnection
            ]
        );
        assert_eq!(
            s.state(),
            &State::Closed {
                reason: CloseReason::GraceExpired
            }
        );
    }

    #[test]
    fn stop_from_streaming_cleans_up() {
        let t0 = Instant::now();
        let mut s = streaming(t0);
        let a = s.handle(Event::StopRequested, t0).unwrap();
        assert!(a.contains(&Action::DestroyVirtualDisplay));
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        let mut s = Session::new(Session::DEFAULT_GRACE);
        assert!(s.handle(Event::Negotiated, Instant::now()).is_err());
        assert!(s.handle(Event::PathRestored, Instant::now()).is_err());
    }

    #[test]
    fn closed_is_terminal() {
        let t0 = Instant::now();
        let mut s = streaming(t0);
        s.handle(Event::StopRequested, t0).unwrap();
        assert!(s.handle(Event::StopRequested, t0).is_err());
    }
}
