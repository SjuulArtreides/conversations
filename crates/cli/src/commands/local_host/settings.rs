use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Preferences for the owner using the shared frontend on localhost.
    #[serde(default)]
    pub ui: LocalPreferences,
    #[serde(default)]
    pub cmux: CmuxSettings,
    /// How this host starts agents for signed remote requests. Central account
    /// settings are never consulted for execution.
    #[serde(default)]
    pub launch: LaunchSettings,
    /// Where passkeys may be used. Both values come from THIS file only; nothing
    /// a server or browser sends can add an origin or change the relying party.
    #[serde(default)]
    pub webauthn: WebauthnSettings,
    /// The central account this host publishes shared history as. Set only
    /// here, by the local user; a login cookie or a signed request never
    /// chooses it.
    #[serde(default)]
    pub publication: PublicationSettings,
    /// Private peer reachability is configured only on this machine.
    #[serde(default)]
    pub transport: TransportSettings,
    /// Persisted startup recipe for the local host daemon: the Arrakis DevKit
    /// checkout and binary lookup directories used by ADK/native launches when
    /// the daemon is started outside the dev shell (e.g. a bare cmux surface or
    /// an upgrade respawn). Local-file only — a signed `LaunchUpdate` cannot
    /// set it, so a server or peer cannot steer this host's process env.
    #[serde(default)]
    pub host_env: HostLaunchEnv,
    /// The owner-only loopback management UI the running host serves.
    /// Local-file only: a signed operation or a server response never enables
    /// it or moves its port.
    #[serde(default, skip_serializing_if = "WebSettings::is_default")]
    pub web: WebSettings,
}

/// The default loopback port of the local management UI.
pub const DEFAULT_WEB_PORT: u16 = 8089;

/// Whether the host serves its loopback management UI, and on which port.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSettings {
    /// Start the web UI when the host starts. An explicit `host web` still
    /// starts a standalone one when this is off.
    #[serde(default = "default_web_enabled")]
    pub enabled: bool,
    /// Loopback port. A port already serving something else is never taken
    /// over: the host starts without the UI and says why.
    #[serde(default = "default_web_port")]
    pub port: u16,
}

fn default_web_enabled() -> bool {
    true
}

fn default_web_port() -> u16 {
    DEFAULT_WEB_PORT
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            enabled: default_web_enabled(),
            port: default_web_port(),
        }
    }
}

impl WebSettings {
    fn is_default(&self) -> bool {
        self.enabled && self.port == DEFAULT_WEB_PORT
    }
}

/// Locally configured startup locations. The checkout is a fallback for a
/// missing ambient `ARRAKIS_HOME`; launcher directories take precedence over
/// ambient `PATH`. Empty settings preserve the ambient environment.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostLaunchEnv {
    /// The Arrakis DevKit checkout. When the daemon starts without `ARRAKIS_HOME`,
    /// this is used as the `adk code` spawn cwd and exported on the child env.
    /// Validated to be an existing directory at startup; an invalid value is
    /// logged and ignored so unrelated launches still work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrakis_home: Option<PathBuf>,
    /// Directories prepended to the `PATH` used only to resolve launcher
    /// binaries (`adk`, `codex`, `claude`, `opencode`). Entries that do not
    /// exist at startup are logged and dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launcher_path: Vec<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationSettings {
    #[serde(default)]
    pub account: Option<String>,
}

/// Private channel transport. Empty `candidate_ips` means this machine's
/// non-loopback interface addresses (at most four).
///
/// `ice_servers` and `ice_transport_policy` use the standard `RTCIceServer` /
/// `RTCConfiguration` shape and apply only to this machine's OUTGOING native
/// agent channels. They are never sent to central or to a peer. Native support
/// is UDP TURN (`turn:host:port[?transport=udp]`); any other scheme or transport
/// is refused when the file is read rather than ignored. Empty means direct
/// candidates only.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportSettings {
    #[serde(default)]
    pub candidate_ips: Vec<std::net::IpAddr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ice_servers: Vec<IceServer>,
    #[serde(default, skip_serializing_if = "IceTransportPolicy::is_all")]
    pub ice_transport_policy: IceTransportPolicy,
}

/// Most ICE servers, and URLs per server, a settings file may name.
pub const MAX_ICE_SERVERS: usize = 4;
pub const MAX_ICE_URLS: usize = 4;
/// Longest URL, username or credential accepted.
pub const MAX_ICE_FIELD: usize = 256;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IceTransportPolicy {
    #[default]
    All,
    /// Only relayed (TURN) candidates: no address of this machine is offered.
    Relay,
}

impl IceTransportPolicy {
    fn is_all(&self) -> bool {
        *self == Self::All
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IceServer {
    pub urls: IceUrls,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<Secret>,
}

impl std::fmt::Debug for IceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls)
            .field("username", &self.username)
            .field("credential", &self.credential)
            .finish()
    }
}

/// `urls` is a string or a list of strings, as in `RTCIceServer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IceUrls {
    One(String),
    Many(Vec<String>),
}

impl IceUrls {
    pub fn list(&self) -> Vec<&str> {
        match self {
            Self::One(url) => vec![url.as_str()],
            Self::Many(urls) => urls.iter().map(String::as_str).collect(),
        }
    }
}

/// A credential that never appears in Debug output or parse errors.
#[derive(Clone, Serialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Through a generic value, so a wrong type never echoes the input.
        match serde_json::Value::deserialize(deserializer) {
            Ok(serde_json::Value::String(value)) => Ok(Self(value)),
            _ => Err(serde::de::Error::custom(
                "an ICE credential must be a string",
            )),
        }
    }
}

/// One TURN server the native client may allocate on, validated.
#[derive(Debug, Clone)]
pub struct TurnServer {
    /// `host:port` as written (resolved when used).
    pub address: String,
    pub username: String,
    pub credential: Secret,
}

impl TransportSettings {
    /// The validated TURN servers for outgoing native channels, and whether
    /// only relayed candidates may be used. Everything unsupported is an error.
    pub fn native_ice(&self) -> Result<(Vec<TurnServer>, bool)> {
        anyhow::ensure!(
            self.ice_servers.len() <= MAX_ICE_SERVERS,
            "at most {MAX_ICE_SERVERS} ICE servers are supported"
        );
        let mut servers = Vec::new();
        for server in &self.ice_servers {
            let urls = server.urls.list();
            anyhow::ensure!(
                (1..=MAX_ICE_URLS).contains(&urls.len()),
                "each ICE server needs 1-{MAX_ICE_URLS} URLs"
            );
            let username = server.username.clone().unwrap_or_default();
            let credential = server.credential.clone();
            anyhow::ensure!(
                (1..=MAX_ICE_FIELD).contains(&username.len())
                    && !username.chars().any(char::is_control),
                "a TURN server needs a username of 1-{MAX_ICE_FIELD} printable characters"
            );
            let Some(credential) = credential.filter(|c| {
                (1..=MAX_ICE_FIELD).contains(&c.0.len()) && !c.0.chars().any(char::is_control)
            }) else {
                anyhow::bail!(
                    "a TURN server needs a credential of 1-{MAX_ICE_FIELD} printable characters"
                );
            };
            for url in urls {
                servers.push(TurnServer {
                    address: turn_udp_address(url)?,
                    username: username.clone(),
                    credential: credential.clone(),
                });
            }
        }
        let relay_only = self.ice_transport_policy == IceTransportPolicy::Relay;
        anyhow::ensure!(
            !relay_only || !servers.is_empty(),
            "ice_transport_policy \"relay\" needs at least one TURN server"
        );
        Ok((servers, relay_only))
    }
}

/// `host:port` of a `turn:` URL over UDP. STUN-only, TLS (`turns:`) and TCP
/// are refused explicitly: the native client does not implement them.
fn turn_udp_address(url: &str) -> Result<String> {
    anyhow::ensure!(
        url.len() <= MAX_ICE_FIELD && !url.chars().any(|c| c.is_control() || c == ' '),
        "an ICE server URL is too long or malformed"
    );
    let rest = match url.split_once(':') {
        Some(("turn", rest)) => rest,
        Some(("stun", _)) => {
            anyhow::bail!(
                "STUN servers are not supported by the native agent client; use a turn: URL"
            )
        }
        Some(("turns", _)) => anyhow::bail!(
            "TURN over TLS (turns:) is not supported by the native agent client; use turn: over UDP"
        ),
        _ => anyhow::bail!("unsupported ICE server URL scheme (only turn: over UDP)"),
    };
    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    match query {
        "" | "transport=udp" => {}
        "transport=tcp" => {
            anyhow::bail!("TURN over TCP is not supported by the native agent client")
        }
        _ => anyhow::bail!("unsupported ICE server URL parameters (only transport=udp)"),
    }
    let (host, port) = address
        .rsplit_once(':')
        .map(|(h, p)| (h, p.parse::<u16>().ok()))
        .unwrap_or((address, Some(3478)));
    let port = port
        .filter(|p| *p != 0)
        .context("invalid TURN server port")?;
    anyhow::ensure!(
        !host.is_empty() && !host.contains(['/', '@']),
        "invalid TURN server host"
    );
    Ok(format!("{host}:{port}"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebauthnSettings {
    /// Relying party id (a registrable domain such as `conversations.example`).
    #[serde(default)]
    pub rp_id: Option<String>,
    /// Exact origins allowed to run ceremonies, e.g. `https://conversations.example`.
    #[serde(default)]
    pub origins: Vec<String>,
}

pub(crate) const CODEX_SANDBOX_MODES: &[&str] =
    &["read-only", "workspace-write", "danger-full-access"];
pub(crate) const CODEX_APPROVAL_POLICIES: &[&str] =
    &["untrusted", "on-failure", "on-request", "never"];

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LaunchSettings {
    #[serde(default)]
    pub claude_skip_permissions: bool,
    #[serde(default)]
    pub opencode_auto: bool,
    #[serde(default)]
    pub codex_sandbox_mode: Option<String>,
    #[serde(default)]
    pub codex_approval_policy: Option<String>,
    /// Harness used when a signed start does not name one.
    #[serde(default)]
    pub default_launcher: conversations_core::AgentLauncher,
    #[serde(default)]
    pub adk_statusline: bool,
}

impl LaunchSettings {
    /// Extra agent CLI arguments. An unrecognized Codex value is an error, so a
    /// hand-edited file cannot smuggle arbitrary words onto the command line.
    pub fn agent_args(&self, agent: conversations_core::AgentKind) -> Result<Vec<String>> {
        use conversations_core::AgentKind;
        let mut args = Vec::new();
        match agent {
            AgentKind::Claude if self.claude_skip_permissions => {
                args.push("--dangerously-skip-permissions".to_owned());
            }
            AgentKind::OpenCode if self.opencode_auto => args.push("--auto".to_owned()),
            AgentKind::Codex => {
                for (flag, value, allowed) in [
                    ("--sandbox", &self.codex_sandbox_mode, CODEX_SANDBOX_MODES),
                    (
                        "--ask-for-approval",
                        &self.codex_approval_policy,
                        CODEX_APPROVAL_POLICIES,
                    ),
                ] {
                    if let Some(value) = value {
                        anyhow::ensure!(
                            allowed.contains(&value.as_str()),
                            "unsupported Codex setting `{value}` in the host settings file"
                        );
                        args.push(flag.to_owned());
                        args.push(value.clone());
                    }
                }
            }
            _ => {}
        }
        Ok(args)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CmuxSettings {
    #[serde(default)]
    pub auto_wrap: bool,
}

static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl From<&super::authz::envelope::LaunchUpdate> for LaunchSettings {
    fn from(update: &super::authz::envelope::LaunchUpdate) -> Self {
        Self {
            claude_skip_permissions: update.claude_skip_permissions,
            opencode_auto: update.opencode_auto,
            codex_sandbox_mode: update.codex_sandbox_mode.clone(),
            codex_approval_policy: update.codex_approval_policy.clone(),
            default_launcher: update.default_launcher,
            adk_statusline: update.adk_statusline,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalPreferences {
    #[serde(default)]
    pub container_paths: Vec<String>,
    #[serde(default = "default_owner_agent")]
    pub default_agent: conversations_core::AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_launcher: Option<conversations_core::AgentLauncher>,
    #[serde(default)]
    pub inline_code_colors: Option<InlineCodeColors>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineCodeColors {
    #[serde(default)]
    pub palette: Vec<u8>,
    #[serde(default)]
    pub rgb: Vec<u32>,
}
fn default_owner_agent() -> conversations_core::AgentKind {
    conversations_core::AgentKind::Claude
}
impl Default for LocalPreferences {
    fn default() -> Self {
        Self {
            container_paths: Vec::new(),
            default_agent: conversations_core::AgentKind::Claude,
            default_launcher: None,
            inline_code_colors: None,
        }
    }
}
/// Set up the host's first passkey origin only after a local-owner action.
/// Existing or partially configured policy is never changed here.
pub(super) fn configure_device_passkeys(dir: &Path, server: &str) -> Result<()> {
    let url = reqwest::Url::parse(server)?;
    anyhow::ensure!(
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
        "Phone setup needs an HTTPS Conversations server."
    );
    let host = url
        .domain()
        .context("Phone setup needs a server domain.")?
        .to_owned();
    let origin = url.origin().ascii_serialization();
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut settings = Settings::load(dir)?;
    anyhow::ensure!(
        settings.webauthn.rp_id.is_none() && settings.webauthn.origins.is_empty(),
        "Passkey settings already exist. Use the configured host and origin."
    );
    settings.webauthn.rp_id = Some(host);
    settings.webauthn.origins = vec![origin];
    super::authz::passkey::relying_party(&settings)?;
    settings.save()
}

pub fn replace_preferences(dir: &Path, preferences: LocalPreferences) -> Result<()> {
    if let Some(colors) = &preferences.inline_code_colors {
        anyhow::ensure!(
            colors.rgb.iter().all(|c| *c <= 0xffffff),
            "invalid RGB color"
        );
    }
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut settings = Settings::load(dir)?;
    settings.ui = preferences;
    settings.save()
}

/// Replace ONLY the launch block of the local settings file, keeping everything
/// else (cmux, passkey origins and relying party) byte-for-byte what the local
/// user configured. Read-modify-write is serialized inside this process.
pub fn replace_launch(dir: &Path, launch: LaunchSettings) -> Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut settings = Settings::load(dir)?;
    settings.launch = launch;
    settings.save()
}

impl Settings {
    pub fn path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("CONVERSATIONS_HOST_SETTINGS_FILE") {
            return Ok(PathBuf::from(path));
        }
        let base = dirs::config_dir().context("could not determine a config directory")?;
        Ok(base.join("arrakis-conversations/local-host.json"))
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path()?;
        match Self::load_file(&path) {
            Ok(Some(settings)) => Ok(settings),
            // Preserve the prototype preference until it is next saved in the
            // stable config location.
            Ok(None) => Ok(Self::load_file(&dir.join("config.json"))?.unwrap_or_default()),
            Err(error) => Err(error),
        }
    }

    /// One settings file, or `None` when it does not exist. A file that holds
    /// an ICE credential must be a regular file owned by this user and not
    /// readable by anyone else; otherwise it is refused (without its content).
    fn load_file(path: &Path) -> Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let settings: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("reading {}", path.display()))?;
        if settings
            .transport
            .ice_servers
            .iter()
            .any(|s| s.credential.is_some())
        {
            owner_only(path)?;
        }
        settings
            .transport
            .native_ice()
            .with_context(|| format!("invalid transport settings in {}", path.display()))?;
        Ok(Some(settings))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let dir = path
            .parent()
            .context("settings file has no parent directory")?;
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        use std::os::unix::fs::OpenOptionsExt;
        let temporary = dir.join(format!(".config-{}.json", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer_pretty(&mut file, self)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &path)?;
            Ok(())
        })();
        let _ = std::fs::remove_file(temporary);
        result
    }
}

#[cfg(unix)]
fn owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        metadata.file_type().is_file() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "refusing ICE credentials in {}: the file must be a regular file owned by you with no group or other access (chmod 600)",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn owner_only(path: &Path) -> Result<()> {
    anyhow::bail!(
        "refusing ICE credentials in {}: owner-only files cannot be checked on this platform",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const CREDENTIAL: &str = "pinecone-credential-7731";

    fn write(dir: &Path, body: serde_json::Value, mode: u32) -> PathBuf {
        let path = dir.join("local-host.json");
        std::fs::write(&path, body.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn turn(url: &str) -> serde_json::Value {
        serde_json::json!({"transport": {"ice_servers": [{"urls": url, "username": "u", "credential": CREDENTIAL}]}})
    }

    #[test]
    fn legacy_and_empty_transport_settings_still_load() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            serde_json::json!({}),
            serde_json::json!({"transport": {"candidate_ips": ["127.0.0.1"]}}),
            serde_json::json!({"launch": {"claude_skip_permissions": true}}),
        ] {
            // Group-readable is fine when nothing secret is in the file.
            let path = write(dir.path(), body, 0o644);
            let settings = Settings::load_file(&path).unwrap().unwrap();
            assert_eq!(settings.transport.native_ice().unwrap().0.len(), 0);
        }
        assert!(Settings::load_file(&dir.path().join("absent.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn web_settings_default_to_on_and_an_opt_out_loads() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing written (a legacy file, or none) keeps the UI starting with
        // the host on the default port, and serializes back without a block.
        let path = write(dir.path(), serde_json::json!({}), 0o644);
        let settings = Settings::load_file(&path).unwrap().unwrap();
        assert_eq!(
            (settings.web.enabled, settings.web.port),
            (true, DEFAULT_WEB_PORT)
        );
        let saved = serde_json::to_value(&settings).unwrap();
        assert!(saved.get("web").is_none(), "{saved}");
        let path = write(
            dir.path(),
            serde_json::json!({"web": {"enabled": false, "port": 19089}}),
            0o644,
        );
        let settings = Settings::load_file(&path).unwrap().unwrap();
        assert_eq!((settings.web.enabled, settings.web.port), (false, 19089));
    }

    #[test]
    fn a_credential_file_readable_by_others_is_refused_without_revealing_it() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o644, 0o640, 0o604, 0o660] {
            let path = write(dir.path(), turn("turn:127.0.0.1:3478"), mode);
            let error = format!("{:#}", Settings::load_file(&path).unwrap_err());
            assert!(error.contains("chmod 600"), "{error}");
            assert!(!error.contains(CREDENTIAL), "{error}");
        }
        let path = write(dir.path(), turn("turn:127.0.0.1:3478"), 0o600);
        let settings = Settings::load_file(&path).unwrap().unwrap();
        let (servers, relay_only) = settings.transport.native_ice().unwrap();
        assert_eq!(servers[0].address, "127.0.0.1:3478");
        assert_eq!(servers[0].credential.expose(), CREDENTIAL);
        assert!(!relay_only);
        // Debug output (logs, panics, error contexts) never shows it.
        for shown in [format!("{settings:?}"), format!("{servers:?}")] {
            assert!(!shown.contains(CREDENTIAL), "{shown}");
            assert!(shown.contains("<redacted>"), "{shown}");
        }
    }

    #[test]
    fn a_symlinked_credential_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = write(dir.path(), turn("turn:127.0.0.1:3478"), 0o600);
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(Settings::load_file(&link).is_err());
    }

    #[test]
    fn a_malformed_credential_never_echoes_its_value() {
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::json!({"transport": {"ice_servers": [{"urls": "turn:h:1", "username": "u", "credential": 77311234}]}});
        let path = write(dir.path(), body, 0o600);
        let error = format!("{:#}", Settings::load_file(&path).unwrap_err());
        assert!(!error.contains("77311234"), "{error}");
    }

    #[test]
    fn unsupported_ice_configuration_is_refused_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        for (url, expected) in [
            ("stun:127.0.0.1:3478", "STUN"),
            ("turns:127.0.0.1:5349", "TLS"),
            ("turn:127.0.0.1:3478?transport=tcp", "TCP"),
            ("turn:127.0.0.1:3478?foo=bar", "parameters"),
            ("http://127.0.0.1", "scheme"),
            ("turn:127.0.0.1:0", "port"),
        ] {
            let path = write(dir.path(), turn(url), 0o600);
            let error = format!("{:#}", Settings::load_file(&path).unwrap_err());
            assert!(error.contains(expected), "{url}: {error}");
            assert!(!error.contains(CREDENTIAL), "{error}");
        }
        let cases = [
            serde_json::json!({"transport": {"ice_transport_policy": "relay"}}),
            serde_json::json!({"transport": {"ice_servers": [{"urls": "turn:h:1", "username": "u"}]}}),
            serde_json::json!({"transport": {"ice_servers": [{"urls": [], "username": "u", "credential": "c"}]}}),
            serde_json::json!({"transport": {"ice_servers": [{"urls": "turn:h:1", "credential": "c"}]}}),
            serde_json::json!({"transport": {"ice_servers": [{"urls": "turn:h:1", "username": "u", "credential": "c", "extra": 1}]}}),
            serde_json::json!({"transport": {"ice_transport_policy": "none"}}),
            serde_json::json!({"transport": {"ice_servers": vec![serde_json::json!({"urls": "turn:h:1", "username": "u", "credential": "c"}); MAX_ICE_SERVERS + 1]}}),
            serde_json::json!({"transport": {"ice_servers": [{"urls": "turn:h:1", "username": "u", "credential": "c".repeat(MAX_ICE_FIELD + 1)}]}}),
        ];
        for body in cases {
            let path = write(dir.path(), body.clone(), 0o600);
            assert!(Settings::load_file(&path).is_err(), "{body}");
        }
        let relay = serde_json::json!({"transport": {"ice_transport_policy": "relay", "ice_servers": [{"urls": ["turn:turn.example:3478?transport=udp"], "username": "u", "credential": "c"}]}});
        let path = write(dir.path(), relay, 0o600);
        let (servers, relay_only) = Settings::load_file(&path)
            .unwrap()
            .unwrap()
            .transport
            .native_ice()
            .unwrap();
        assert_eq!(
            (servers[0].address.as_str(), relay_only),
            ("turn.example:3478", true)
        );
    }
}
