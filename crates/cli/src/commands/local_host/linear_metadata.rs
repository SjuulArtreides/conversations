//! Session-bound Linear metadata operations. No external Linear API is used.
use super::{
    authz::{
        dispatch,
        envelope::{Operation, Verified},
    },
    history::History,
};
use anyhow::{ensure, Result};
use std::path::Path;

pub(super) fn unlink_signed(dir: &Path, op: &Verified) -> Result<()> {
    let Operation::LinearUnlink { session, url } = &op.op else {
        anyhow::bail!("not a Linear unlink");
    };
    let mut history = History::open(dir)?;
    dispatch::begin(dir, op)?;
    ensure!(
        history.unlink_linear(session, url)?,
        "Unknown Linear reference"
    );
    Ok(())
}
