//! Public metadata of keys already enrolled on this host, never key material.
use super::authz::store::open_private;
use anyhow::Result;
use conversations_core::private_channel::{AccessCredential, AccessGrant};
use std::path::Path;

pub(super) fn list(dir: &Path) -> Result<Vec<AccessCredential>> {
    let conn = open_private(dir, "authz.db")?;
    let mut stmt = conn.prepare("SELECT cred_id,label,alg,kind,revoked_at FROM credentials ORDER BY approved_at DESC,cred_id")?;
    let mut credentials = Vec::new();
    for row in stmt.query_map([], |row| {
        Ok(AccessCredential {
            kid: row.get(0)?,
            label: row.get(1)?,
            alg: row.get(2)?,
            kind: row.get(3)?,
            revoked: row.get::<_, Option<i64>>(4)?.is_some(),
            grants: Vec::new(),
        })
    })? {
        let mut credential = row?;
        credential.grants = conn.prepare("SELECT cap,session_id FROM grants WHERE cred_id=?1 AND revoked_at IS NULL ORDER BY cap,session_id")?
            .query_map([&credential.kid], |row| Ok(AccessGrant {cap:row.get(0)?,session:row.get(1)?}))?
            .collect::<rusqlite::Result<_>>()?;
        credentials.push(credential);
    }
    Ok(credentials)
}
