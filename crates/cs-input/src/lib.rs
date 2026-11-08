//! Captura e inyección de entrada remota (F6: ratón y teclado).
//!
//! Este crate traduce eventos del protocolo (`cs-protocol`) a inyección del SO
//! en el emisor (`SendInput` en Windows) acotada al rectángulo del monitor
//! transmitido.
