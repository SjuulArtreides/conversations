//! Where `live_error!` and `live_info!` diagnostics go.
use std::fmt;

pub(crate) fn report(args: fmt::Arguments<'_>, stderr: bool) {
    if stderr {
        eprintln!("{args}");
    } else {
        println!("{args}");
    }
}
