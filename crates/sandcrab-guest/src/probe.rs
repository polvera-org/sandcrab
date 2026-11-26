//! `crabd probe`: the in-sandbox half of the self-test. Not implemented yet; the `selftest`
//! step fills it in.

use std::{ffi::OsString, process::ExitCode};

/// Entry point of `crabd probe`, called with the arguments after `probe`.
pub fn main(_args: &[OsString]) -> ExitCode {
    eprintln!("crabd probe: not implemented");
    ExitCode::FAILURE
}
