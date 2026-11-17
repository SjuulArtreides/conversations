//! `conversations ping` — probe the server's /healthz endpoint.

use anyhow::Result;

use crate::cli::GlobalOpts;
use crate::client::ApiClient;

pub fn run(global: &GlobalOpts) -> Result<()> {
    let client = ApiClient::new(&global.server)?;
    match client.healthz() {
        Ok(h) if h.status == "ok" => {
            println!("{} is reachable (status: {})", global.server, h.status);
            Ok(())
        }
        Ok(h) => {
            anyhow::bail!(
                "{} responded but reported status {:?} (expected \"ok\")",
                global.server,
                h.status
            );
        }
        Err(e) => Err(e.context(format!("{} is NOT reachable", global.server))),
    }
}
