//! Turning measurements into conclusions.
//!
//! The full engine described in the plan compares against three evidence layers:
//! silicon specs, model cohorts, and vendor reference data. None of that is needed for
//! the read-only inventory, because the rules here rest on facts that are true of any
//! machine: a drive reporting unreadable sectors is failing, firmware-persistent
//! anti-theft is a liability, memory running below its own rated speed is misconfigured.
//!
//! Every rule states what it saw, what it expected, and where that expectation comes
//! from. A buyer has to defend these conclusions to a seller, so a finding that cannot
//! explain itself does not belong here.

pub mod findings;
pub mod stress_findings;

pub use findings::evaluate;
