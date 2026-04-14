//! Detector functions for the phase monitor.
//!
//! Each detector evaluates one concern (idle, stuck, crashed, etc.) and
//! returns zero or more [`MonitorEvent`](super::MonitorEvent)s. The
//! [`DefaultPhaseMonitor`](super::DefaultPhaseMonitor) composes these
//! detectors to implement the full evaluation tick.
