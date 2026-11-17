//! Authority for clipboard images and file drops.
//!
//! The central server only CARRIES the bytes. Whether a wrapper may put an image
//! on the clipboard or stage files and paste their paths is decided by this host:
//! the browser signs the exact type/size/SHA-256 (names/sizes/hashes for files),
//! the host verifies it against its local grants, and only then tells THIS
//! terminal's wrapper (`ServerMessage::TransferAuthorized`). The wrapper's
//! consumers call [`TransferTable::claim_image`] / [`TransferTable::claim_files`]
//! with the bytes they actually received; with no matching, unexpired,
//! unconsumed authorization the answer is `None` and nothing happens. A claim
//! consumes the authorization, so identical bytes cannot be applied twice.
//!
//! A claim is not the last authority check: it hands out a [`Permit`] carrying
//! the signed authority, and the paste re-checks it against the current local
//! rows under the terminal writer lock, immediately before writing. A
//! revocation committed after admission therefore stops a staged paste.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use conversations_core::terminal_files::{
    validate_file_name, TerminalFile, IMAGE_MIMES, MAX_FILES, MAX_IMAGE_BYTES, MAX_TOTAL_BYTES,
};
use ring::digest;
use serde::{Deserialize, Serialize};

/// Longest a verified authorization stays usable. The host states an absolute
/// deadline (never later than the signed operation's own expiry); this is only the cap.
pub const AUTH_TTL: Duration = Duration::from_secs(60);

/// Authorizations a wrapper holds at once.
const MAX_OPEN: usize = 8;
/// Recently seen transfer ids, to refuse a re-sent id.
const SEEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferItem {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransferKind {
    Image {
        mime: String,
        size: u64,
        sha256: String,
    },
    Files {
        files: Vec<TransferItem>,
    },
}

impl TransferKind {
    /// Bytes this authorization covers.
    pub fn bytes(&self) -> u64 {
        match self {
            TransferKind::Image { size, .. } => *size,
            TransferKind::Files { files } => files.iter().map(|f| f.size).sum(),
        }
    }

    /// The bounds the signed operation already enforced, checked again here: this
    /// is the last place before an effect.
    fn well_formed(&self) -> bool {
        let hex = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        match self {
            TransferKind::Image { mime, size, sha256 } => {
                IMAGE_MIMES.contains(&mime.as_str())
                    && (1..=MAX_IMAGE_BYTES as u64).contains(size)
                    && hex(sha256)
            }
            TransferKind::Files { files } => {
                !files.is_empty()
                    && files.len() <= MAX_FILES
                    && files.iter().all(|f| {
                        validate_file_name(&f.name).is_ok()
                            && !f.name.contains(['\u{2028}', '\u{2029}'])
                            && hex(&f.sha256)
                    })
                    && self.bytes() <= MAX_TOTAL_BYTES as u64
            }
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    digest::digest(&digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The authority one claimed transfer still needs at its final write.
#[derive(Clone, Default)]
pub struct Permit(
    Option<(
        Option<Arc<super::authz::cutover::Gate>>,
        super::authz::cutover::Authority,
    )>,
);

impl Permit {
    /// `None` = the paste may happen now. A transfer that names an authority on
    /// a table with no local check fails closed.
    pub fn denial(&self) -> Option<String> {
        let (gate, authority) = self.0.as_ref()?;
        match gate {
            Some(gate) => gate.denial(Some(authority)),
            None => Some("no local authority check".into()),
        }
    }
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Permit")
            .field(&self.0.as_ref().map(|(_, a)| a))
            .finish()
    }
}

/// A consumed authorization: how long it had left, and what it still needs.
#[derive(Debug, Clone)]
pub struct Claim {
    pub left: Duration,
    pub permit: Permit,
}

struct Open {
    transfer: String,
    kind: TransferKind,
    authority: Option<super::authz::cutover::Authority>,
    /// Local elapsed-time deadline, fixed on receipt from the host's absolute one.
    expires: Instant,
    /// The host's absolute deadline. A claim needs BOTH: a wall-clock step back
    /// cannot lengthen the permit, and a suspend cannot hide its expiry.
    expires_unix_ms: u64,
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The absolute deadline `ttl` from now, for the host to send.
pub fn deadline_in(ttl: Duration) -> u64 {
    unix_ms() + ttl.as_millis() as u64
}

#[derive(Default)]
struct Inner {
    open: Vec<Open>,
    seen: VecDeque<String>,
}

/// Wrapper side: what this terminal may apply right now.
#[derive(Default)]
pub struct TransferTable {
    inner: Mutex<Inner>,
    /// Effect-time authority check for permits.
    gate: Option<Arc<super::authz::cutover::Gate>>,
}

impl TransferTable {
    /// A table whose permits re-check their authority through `gate`.
    pub fn gated(gate: Arc<super::authz::cutover::Gate>) -> Self {
        Self {
            gate: Some(gate),
            ..Self::default()
        }
    }

    /// Record an authorization the host verified, without signed authority
    /// (so its paste is refused at the final write). Test-only.
    #[cfg(test)]
    pub fn authorize(
        &self,
        transfer: &str,
        kind: TransferKind,
        expires_unix_ms: u64,
    ) -> Result<(), &'static str> {
        self.authorize_as(transfer, kind, expires_unix_ms, None)
    }

    /// Record an authorization the host verified, with the signed authority the
    /// paste must still hold when it is finally written. Refuses a malformed
    /// one, a re-used transfer id, and a full table.
    pub fn authorize_as(
        &self,
        transfer: &str,
        kind: TransferKind,
        expires_unix_ms: u64,
        authority: Option<super::authz::cutover::Authority>,
    ) -> Result<(), &'static str> {
        if !(16..=64).contains(&transfer.len()) || !transfer.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid transfer id");
        }
        if !kind.well_formed() {
            return Err("invalid transfer");
        }
        // Delay between the host's decision and now only SHORTENS the permit.
        let left = Duration::from_millis(expires_unix_ms.saturating_sub(unix_ms())).min(AUTH_TTL);
        if left.is_zero() {
            return Err("authorization already expired");
        }
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner.open.retain(|o| o.expires > now);
        if inner.seen.iter().any(|s| s == transfer) {
            return Err("transfer id already used");
        }
        if inner.open.len() >= MAX_OPEN {
            return Err("too many pending transfers");
        }
        if inner.seen.len() >= SEEN {
            inner.seen.pop_front();
        }
        inner.seen.push_back(transfer.to_owned());
        inner.open.push(Open {
            transfer: transfer.to_owned(),
            kind,
            authority,
            expires: now + left,
            expires_unix_ms,
        });
        Ok(())
    }

    fn take(&self, transfer: &str, matches: impl Fn(&TransferKind) -> bool) -> Option<Claim> {
        self.take_at(transfer, matches, Instant::now(), unix_ms())
    }

    /// The claim's lifetime is what is left by BOTH clocks: the monotonic one and
    /// the host's absolute deadline. Either may be the shorter (a paused monotonic
    /// clock, a wall clock stepped forward), and the result never exceeds either.
    /// Exactly-at-expiry on either clock is expired.
    fn take_at(
        &self,
        transfer: &str,
        matches: impl Fn(&TransferKind) -> bool,
        now: Instant,
        wall: u64,
    ) -> Option<Claim> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .open
            .retain(|o| o.expires > now && o.expires_unix_ms > wall);
        let at = inner
            .open
            .iter()
            .position(|o| o.transfer == transfer && matches(&o.kind))?;
        let open = inner.open.remove(at);
        let mono_left = open.expires.saturating_duration_since(now);
        let wall_left = Duration::from_millis(open.expires_unix_ms - wall);
        let left = Some(mono_left.min(wall_left)).filter(|left| !left.is_zero())?;
        let permit = Permit(
            open.authority
                .map(|authority| (self.gate.clone(), authority)),
        );
        Some(Claim { left, permit })
    }

    /// Consume the authorization for exactly these image bytes, if there is one.
    /// Returns how long the authorization had left.
    pub fn claim_image(&self, transfer: &str, mime: &str, bytes: &[u8]) -> Option<Claim> {
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return None;
        }
        let hash = sha256_hex(bytes);
        self.take(transfer, |kind| {
            matches!(kind, TransferKind::Image { mime: m, size, sha256 }
                if m == mime && *size == bytes.len() as u64 && *sha256 == hash)
        })
    }

    /// Consume the authorization for exactly this batch (names, decoded sizes and
    /// SHA-256s, in order), if there is one.
    pub fn claim_files(&self, transfer: &str, files: &[TerminalFile]) -> Option<Claim> {
        let engine = base64::engine::general_purpose::STANDARD;
        // Only decode (and hash) the batch against candidates that already agree
        // on count and names: at most MAX_OPEN of them.
        let candidates: Vec<usize> = {
            let now = Instant::now();
            let inner = self.inner.lock().unwrap();
            inner
                .open
                .iter()
                .enumerate()
                .filter(|(_, o)| o.expires > now && o.transfer == transfer)
                .filter(|(_, o)| {
                    matches!(&o.kind, TransferKind::Files { files: want }
                        if want.len() == files.len()
                            && want.iter().zip(files).all(|(w, f)| w.name == f.name))
                })
                .map(|(i, _)| i)
                .collect()
        };
        if candidates.is_empty() {
            return None;
        }
        let mut measured = Vec::with_capacity(files.len());
        let mut total = 0usize;
        for file in files {
            if file.b64.len() / 4 * 3 > MAX_TOTAL_BYTES + 3 {
                return None;
            }
            let bytes = engine.decode(&file.b64).ok()?;
            total += bytes.len();
            if total > MAX_TOTAL_BYTES {
                return None;
            }
            measured.push((bytes.len() as u64, sha256_hex(&bytes)));
        }
        self.take(transfer, |kind| {
            matches!(kind, TransferKind::Files { files: want }
            if want.len() == measured.len()
                && want.iter().zip(files).zip(&measured).all(|((w, f), (size, hash))| {
                    w.name == f.name && w.size == *size && w.sha256 == *hash
                }))
        })
    }
}

struct Reserved {
    transfer: String,
    session: String,
    kid: String,
    bytes: u64,
    expires: Instant,
}

/// Daemon side: bounds how much authorization one key, one session and the whole
/// host can have outstanding. Entries age out with the authorization itself.
#[derive(Default)]
pub struct TransferLedger {
    held: Mutex<Vec<Reserved>>,
}

impl TransferLedger {
    const PER_KEY: usize = 16;
    const PER_SESSION: usize = 8;
    const GLOBAL: usize = 64;
    const PER_SESSION_BYTES: u64 = 4 * MAX_TOTAL_BYTES as u64;

    pub fn reserve(
        &self,
        kid: &str,
        session: &str,
        transfer: &str,
        bytes: u64,
    ) -> Result<(), &'static str> {
        let now = Instant::now();
        let mut held = self.held.lock().unwrap();
        held.retain(|r| r.expires > now);
        if held.iter().any(|r| r.transfer == transfer) {
            return Err("transfer id already used");
        }
        if held.len() >= Self::GLOBAL {
            return Err("host has too many pending transfers");
        }
        if held.iter().filter(|r| r.kid == kid).count() >= Self::PER_KEY {
            return Err("this key has too many pending transfers");
        }
        let (count, sum) = held
            .iter()
            .filter(|r| r.session == session)
            .fold((0, 0u64), |(c, s), r| (c + 1, s + r.bytes));
        if count >= Self::PER_SESSION || sum + bytes > Self::PER_SESSION_BYTES {
            return Err("this session has too many pending transfers");
        }
        held.push(Reserved {
            transfer: transfer.to_owned(),
            session: session.to_owned(),
            kid: kid.to_owned(),
            bytes,
            expires: now + AUTH_TTL,
        });
        Ok(())
    }

    pub fn release(&self, transfer: &str) {
        self.held.lock().unwrap().retain(|r| r.transfer != transfer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "00000000000000000000000000000001";
    const T2: &str = "00000000000000000000000000000002";

    fn live() -> u64 {
        deadline_in(AUTH_TTL)
    }

    fn image(bytes: &[u8]) -> TransferKind {
        TransferKind::Image {
            mime: "image/png".into(),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
        }
    }

    fn file(name: &str, bytes: &[u8]) -> (TransferItem, TerminalFile) {
        (
            TransferItem {
                name: name.into(),
                size: bytes.len() as u64,
                sha256: sha256_hex(bytes),
            },
            TerminalFile {
                name: name.into(),
                b64: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        )
    }

    #[test]
    fn an_image_claim_needs_the_id_type_size_and_hash_and_is_one_shot() {
        let table = TransferTable::default();
        table.authorize(T1, image(b"png-bytes"), live()).unwrap();
        assert!(table.claim_image(T1, "image/jpeg", b"png-bytes").is_none());
        assert!(table.claim_image(T1, "image/png", b"png-byteZ").is_none());
        assert!(table.claim_image(T1, "image/png", b"png-bytes!").is_none());
        assert!(table.claim_image(T1, "image/png", b"").is_none());
        assert!(table.claim_image(T2, "image/png", b"png-bytes").is_none());
        assert!(table.claim_image("", "image/png", b"png-bytes").is_none());
        assert!(table.claim_image(T1, "image/png", b"png-bytes").is_some());
        assert!(
            table.claim_image(T1, "image/png", b"png-bytes").is_none(),
            "replayed bytes"
        );
    }

    #[test]
    fn identical_bytes_under_another_id_do_not_consume_a_different_authorization() {
        let table = TransferTable::default();
        table.authorize(T1, image(b"same"), live()).unwrap();
        table.authorize(T2, image(b"same"), live()).unwrap();
        // The second authorization is used first: the first stays pending.
        assert!(table.claim_image(T2, "image/png", b"same").is_some());
        assert!(table.claim_image(T2, "image/png", b"same").is_none());
        assert!(table
            .claim_image("ffffffffffffffffffffffffffffffff", "image/png", b"same")
            .is_none());
        assert!(table.claim_image(T1, "image/png", b"same").is_some());
    }

    #[test]
    fn a_files_claim_needs_id_names_sizes_hashes_count_and_order() {
        let (a_item, a) = file("a.txt", b"alpha");
        let (b_item, b) = file("b.txt", b"beta");
        let want = TransferKind::Files {
            files: vec![a_item.clone(), b_item.clone()],
        };
        let table = TransferTable::default();
        table.authorize(T1, want, live()).unwrap();
        let (_, b_other) = file("b.txt", b"BETA");
        let (_, a_renamed) = file("c.txt", b"alpha");
        for bad in [
            vec![a.clone()],
            vec![b.clone(), a.clone()],
            vec![a.clone(), b_other],
            vec![a_renamed, b.clone()],
            vec![a.clone(), b.clone(), a.clone()],
        ] {
            assert!(table.claim_files(T1, &bad).is_none());
        }
        assert!(table.claim_files(T2, &[a.clone(), b.clone()]).is_none());
        assert!(table.claim_files(T1, &[a.clone(), b.clone()]).is_some());
        assert!(table.claim_files(T1, &[a, b]).is_none(), "replayed batch");
    }

    #[test]
    fn an_expired_authorization_claims_nothing() {
        let table = TransferTable::default();
        table
            .authorize(T1, image(b"x"), deadline_in(Duration::from_millis(80)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(160));
        assert!(table.claim_image(T1, "image/png", b"x").is_none());
    }

    #[test]
    fn delayed_delivery_to_the_wrapper_shortens_the_permit_and_never_renews_it() {
        let table = TransferTable::default();
        // The host fixed a deadline 300 ms out; the message sat in a queue for 200 ms.
        let deadline = deadline_in(Duration::from_millis(300));
        std::thread::sleep(Duration::from_millis(200));
        table.authorize(T1, image(b"x"), deadline).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            table.claim_image(T1, "image/png", b"x").is_none(),
            "not given a fresh lifetime on receipt"
        );
        // A deadline already past at receipt is refused outright.
        assert_eq!(
            table.authorize(T2, image(b"x"), unix_ms().saturating_sub(1)),
            Err("authorization already expired")
        );
        // A far-future deadline is capped, never honoured beyond AUTH_TTL.
        let far = TransferTable::default();
        far.authorize(T1, image(b"x"), deadline_in(Duration::from_secs(86_400)))
            .unwrap();
        let left = far.claim_image(T1, "image/png", b"x").unwrap().left;
        assert!(left <= AUTH_TTL);
    }

    #[test]
    fn ids_are_not_reusable_and_the_table_is_bounded() {
        let table = TransferTable::default();
        table.authorize(T1, image(b"x"), live()).unwrap();
        assert_eq!(
            table.authorize(T1, image(b"y"), live()),
            Err("transfer id already used")
        );
        assert!(table.claim_image(T1, "image/png", b"x").is_some());
        assert_eq!(
            table.authorize(T1, image(b"x"), live()),
            Err("transfer id already used"),
            "a consumed id stays spent"
        );
        for i in 0..MAX_OPEN {
            table
                .authorize(&format!("{:032x}", 100 + i), image(b"z"), live())
                .unwrap();
        }
        assert_eq!(
            table.authorize(T2, image(b"z"), live()),
            Err("too many pending transfers")
        );
    }

    #[test]
    fn malformed_authorizations_are_refused_at_the_last_boundary() {
        let table = TransferTable::default();
        let bad_name = TransferKind::Files {
            files: vec![TransferItem {
                name: "../etc/passwd".into(),
                size: 1,
                sha256: sha256_hex(b"x"),
            }],
        };
        let bad_mime = TransferKind::Image {
            mime: "text/html".into(),
            size: 1,
            sha256: sha256_hex(b"x"),
        };
        let huge = TransferKind::Image {
            mime: "image/png".into(),
            size: MAX_IMAGE_BYTES as u64 + 1,
            sha256: sha256_hex(b"x"),
        };
        for kind in [bad_name, bad_mime, huge] {
            assert_eq!(table.authorize(T1, kind, live()), Err("invalid transfer"));
        }
        assert_eq!(
            table.authorize("short", image(b"x"), live()),
            Err("invalid transfer id")
        );
    }

    #[test]
    fn the_ledger_bounds_keys_sessions_and_the_host() {
        let ledger = TransferLedger::default();
        for i in 0..TransferLedger::PER_SESSION {
            ledger.reserve("k", "s1", &format!("a{i}"), 1).unwrap();
        }
        assert!(ledger.reserve("k", "s1", "over", 1).is_err());
        assert!(ledger.reserve("k", "s1", "a0", 1).is_err(), "duplicate id");
        ledger.release("a0");
        ledger.reserve("k", "s1", "again", 1).unwrap();
        for i in 0..8 {
            ledger.reserve("k", "s2", &format!("b{i}"), 1).unwrap();
        }
        assert_eq!(
            ledger.reserve("k", "s3", "c", 1),
            Err("this key has too many pending transfers")
        );
        assert!(ledger
            .reserve("other", "s9", "big", TransferLedger::PER_SESSION_BYTES + 1)
            .is_err());
    }

    #[test]
    fn a_claim_never_outlives_the_absolute_deadline_even_when_monotonic_time_lags() {
        let t = TransferTable::default();
        let bytes = b"img";
        let expires = deadline_in(AUTH_TTL);
        t.authorize(T1, image(bytes), expires).unwrap();
        // Monotonic clock says ~60 s remain; the wall clock has advanced to 100 ms
        // before the signed absolute expiry (suspend, or a forward step).
        let left = t
            .take_at(T1, |_| true, Instant::now(), expires - 100)
            .expect("still valid by both clocks")
            .left;
        assert!(
            left <= Duration::from_millis(100),
            "claim lifetime {left:?} exceeds the 100 ms the absolute deadline allows"
        );
        assert!(!left.is_zero());
    }

    #[test]
    fn the_monotonic_budget_still_bounds_a_claim_when_the_wall_has_more() {
        let t = TransferTable::default();
        let expires = deadline_in(AUTH_TTL);
        t.authorize(T1, image(b"x"), expires).unwrap();
        // Wall clock stepped BACK an hour: the absolute budget grows, the
        // monotonic one (about 60 s) must still cap the claim.
        let left = t
            .take_at(T1, |_| true, Instant::now(), expires - 3_600_000)
            .unwrap()
            .left;
        assert!(left <= AUTH_TTL);
    }

    #[test]
    fn expiry_exactly_at_the_boundary_or_after_is_refused_and_consumes_nothing_valid() {
        for offset in [0u64, 1, 5_000] {
            let t = TransferTable::default();
            let expires = deadline_in(AUTH_TTL);
            t.authorize(T1, image(b"x"), expires).unwrap();
            assert!(
                t.take_at(T1, |_| true, Instant::now(), expires + offset)
                    .is_none(),
                "wall {offset} ms past/at the signed expiry"
            );
        }
        let t = TransferTable::default();
        let expires = deadline_in(AUTH_TTL);
        t.authorize(T1, image(b"x"), expires).unwrap();
        let later = Instant::now() + AUTH_TTL;
        assert!(
            t.take_at(T1, |_| true, later, expires - 1).is_none(),
            "monotonic expiry"
        );
    }
}
