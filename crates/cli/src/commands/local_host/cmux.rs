use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

use super::protocol::Launch;

/// Marks a launch failure that happened before anything was sent to cmux.
#[derive(Debug)]
pub struct NotStarted;

impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing was started")
    }
}

pub(super) const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone)]
pub struct Cmux {
    binary: PathBuf,
    pub host_workspace: String,
}

pub(super) struct WorkspaceOrder {
    pub window: String,
    pub ids: Vec<String>,
    pub pinned: Vec<bool>,
}

impl Cmux {
    #[cfg(test)]
    pub(super) fn test_backend(binary: PathBuf) -> Self {
        Self {
            binary,
            host_workspace: "host".into(),
        }
    }

    /// A restored pane can exist without a PTY. cmux's socket read starts that
    /// runtime headlessly and waits for it, without focus changes or keystrokes.
    /// Discard the bounded screen response; Conversations owns its own replay.
    pub async fn start_saved_surface(&self, workspace: &str, surface: &str) -> Result<()> {
        self.rpc(
            "surface.read_text",
            json!({"workspace_id": workspace, "surface_id": surface, "lines": 1}),
        )
        .await?;
        Ok(())
    }

    /// cmux's hidden bootstrap grid is a placeholder, not the last visible pane.
    pub async fn is_background_surface(&self, surface: &str) -> Result<bool> {
        let response = self.rpc("debug.terminals", json!({})).await?;
        Ok(response["terminals"]
            .as_array()
            .and_then(|terminals| {
                terminals.iter().find(|terminal| {
                    terminal["surface_id"]
                        .as_str()
                        .is_some_and(|id| id.eq_ignore_ascii_case(surface))
                })
            })
            .is_some_and(|terminal| {
                terminal["hosted_view_in_headless_bootstrap_window"].as_bool() == Some(true)
                    || terminal["hosted_view_visible_in_ui"].as_bool() == Some(false)
            }))
    }

    pub async fn saved_session_for_surface(
        &self,
        runtime: &Path,
        server: &str,
        surface: &str,
    ) -> Result<Option<crate::open_sessions::OpenSession>> {
        super::authz::sessions::import_once(runtime, server)?;
        let binding = self.resume_binding(surface).await?;
        let saved = super::authz::sessions::restore(runtime, server)?;
        Ok(binding.as_ref().and_then(|binding| {
            let checkpoint = binding["checkpoint_id"].as_str()?;
            let kind = binding["kind"].as_str()?;
            saved.into_iter().find(|saved| {
                super::authz::restorable(runtime, &saved.session_id)
                    && ((kind == "conversations" && saved.session_id == checkpoint)
                        || (saved.agent.as_str() == kind
                            && saved.resume_id.as_deref().unwrap_or(&saved.session_id)
                                == checkpoint))
            })
        }))
    }

    pub async fn resume_binding(&self, surface: &str) -> Result<Option<Value>> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(&self.binary)
                .args(["surface", "resume", "show", "--json", "--surface", surface])
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        ensure!(
            output.status.success(),
            "reading cmux resume metadata failed"
        );
        let metadata: Value = serde_json::from_slice(&output.stdout)?;
        let binding = metadata
            .get("resume_binding")
            .filter(|v| !v.is_null())
            .or_else(|| metadata.get("restore_record"));
        Ok(binding.cloned())
    }

    /// cmux persists pane metadata; Conversations remains the owner of argv and
    /// resume policy. This binding identifies a pane even without agent hooks.
    pub async fn bind_saved_session(
        &self,
        runtime: &Path,
        server: &str,
        surface: &str,
        id: &str,
    ) -> Result<()> {
        let binary = std::env::current_exe()?;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(&self.binary)
                .args([
                    "surface",
                    "resume",
                    "set",
                    "--surface",
                    surface,
                    "--kind",
                    "conversations",
                    "--checkpoint",
                    id,
                    "--source",
                    "conversations",
                    "--",
                ])
                .arg(binary)
                .args(["--server", server, "host", "--runtime-dir"])
                .arg(runtime)
                .arg("shell")
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        ensure!(
            output.status.success(),
            "binding Conversations restore metadata: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    pub async fn clear_resume_binding(&self, surface: &str) -> Result<()> {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(&self.binary)
                .args(["surface", "resume", "clear", "--surface", surface])
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        ensure!(
            output.status.success(),
            "clearing host resume metadata failed"
        );
        Ok(())
    }

    pub async fn connect(binary: Option<&Path>) -> Result<Self> {
        let host_workspace = std::env::var("CMUX_WORKSPACE_ID").context(
            "cmux backend must run inside a cmux terminal; launch `host run` in a dedicated pane",
        )?;
        Self::connect_to(binary, host_workspace).await
    }

    /// Like `connect`, but with the host workspace supplied by the caller so
    /// backend auto-detection can test a specific CMUX_WORKSPACE_ID without
    /// the ambient environment.
    pub async fn connect_to(binary: Option<&Path>, host_workspace: String) -> Result<Self> {
        let binary = binary
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("CMUX_BUNDLED_CLI_PATH").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("cmux"));
        let cmux = Self {
            binary,
            host_workspace,
        };
        cmux.rpc("system.ping", json!({})).await?;
        Ok(cmux)
    }

    /// Whether the configured `host_workspace` is still in the live tree. Auto
    /// detection uses this to refuse a stale inherited workspace id, rather
    /// than binding the host to whatever workspace cmux happens to show first.
    pub async fn host_workspace_present(&self) -> Result<bool> {
        let workspace = self.host_workspace.to_ascii_lowercase();
        Ok(self
            .workspace_order()
            .await?
            .into_iter()
            .any(|window| window.ids.contains(&workspace)))
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let output = tokio::process::Command::new(&self.binary)
            .args(["rpc", method, &params.to_string()])
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(RPC_TIMEOUT, output).await??;
        ensure!(
            output.status.success(),
            "cmux {method}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value =
            serde_json::from_slice(&output.stdout).context("cmux returned invalid JSON")?;
        if response.get("ok") == Some(&Value::Bool(false)) {
            bail!("cmux {method}: {response}");
        }
        Ok(response.get("result").cloned().unwrap_or(response))
    }

    async fn create_workspace(&self, mut params: Value) -> Result<Value> {
        let window = self
            .workspace_order()
            .await?
            .into_iter()
            .find(|window| {
                window
                    .ids
                    .iter()
                    .any(|id| id.eq_ignore_ascii_case(&self.host_workspace))
            })
            .context(
                "cmux host workspace is no longer present; start the host in an existing workspace",
            )?;
        params["window_id"] = window.window.into();
        // Resolve the window on each launch: restored or moved workspaces may
        // no longer belong to the window inherited by the host process.
        self.rpc("workspace.create", params).await
    }

    /// Query cmux instead of trusting the terminal's inherited workspace ID:
    /// a surface can move without its running process environment changing.
    pub async fn surface_workspace(&self, surface: &str) -> Result<String> {
        self.surface_workspaces()
            .await?
            .remove(&surface.to_ascii_lowercase())
            .with_context(|| format!("cmux surface {surface} is no longer present"))
    }

    /// One cmux tree lookup is sufficient for all active terminal identities.
    pub async fn surface_workspaces(&self) -> Result<std::collections::HashMap<String, String>> {
        let output = tokio::process::Command::new(&self.binary)
            .args(["--json", "--id-format", "uuids", "tree", "--all"])
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(RPC_TIMEOUT, output).await??;
        ensure!(
            output.status.success(),
            "cmux tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let tree: Value = serde_json::from_slice(&output.stdout)?;
        let mut surfaces = std::collections::HashMap::new();
        for window in tree["windows"].as_array().context("missing cmux windows")? {
            for workspace in window["workspaces"]
                .as_array()
                .context("missing cmux workspaces")?
            {
                for pane in workspace["panes"]
                    .as_array()
                    .context("missing cmux panes")?
                {
                    for item in pane["surfaces"]
                        .as_array()
                        .context("missing cmux surfaces")?
                    {
                        // Resume metadata belongs to terminals. Browser and other
                        // typed panels cannot own a saved terminal checkpoint.
                        // Keep untyped entries conservative for older cmux builds.
                        if item["type"].as_str().is_some_and(|kind| kind != "terminal") {
                            continue;
                        }
                        if let (Some(surface), Some(workspace)) =
                            (item["id"].as_str(), workspace["id"].as_str())
                        {
                            surfaces.insert(surface.to_ascii_lowercase(), workspace.to_owned());
                        }
                    }
                }
            }
        }
        Ok(surfaces)
    }

    pub async fn separate_surface(
        &self,
        surface: &str,
        runtime_dir: &Path,
        launch: &Launch,
    ) -> Result<String> {
        // The built-in move-to-new-workspace drops workspace environment.
        // Create a configured destination first, then move the existing shell.
        let bootstrap = write_zsh_bootstrap(runtime_dir, &std::env::current_exe()?)?;
        let created = self
            .create_workspace(
                json!({
                    "title": launch.title(), "working_directory": launch.cwd,
                    "workspace_env": {"GHOSTTY_ZSH_ZDOTDIR": bootstrap, "CONVERSATIONS_CAPTURE": "1", "CONVERSATIONS_HOST_DIR": runtime_dir},
                    "initial_command": "/bin/sleep 60", "focus": false,
                }),
            )
            .await?;
        let destination = created["workspace_id"]
            .as_str()
            .context("missing new workspace ID")?;
        let placeholder = created["surface_id"]
            .as_str()
            .context("missing placeholder surface ID")?;
        let move_result = async {
            let output = tokio::process::Command::new(&self.binary)
                .args([
                    "move-surface",
                    "--surface",
                    surface,
                    "--workspace",
                    destination,
                    "--focus",
                    "false",
                ])
                .kill_on_drop(true)
                .output();
            let output = tokio::time::timeout(RPC_TIMEOUT, output).await??;
            ensure!(
                output.status.success(),
                "cmux move surface: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let current = self.surface_workspace(surface).await?;
                    if current.eq_ignore_ascii_case(destination) {
                        return Ok::<(), anyhow::Error>(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
            .await
            .context("cmux did not finish moving the terminal")??;
            // Only close the inert surface we created, after the real shell arrived.
            if let Err(error) = self
                .rpc(
                    "surface.close",
                    json!({"surface_id": placeholder, "workspace_id": destination, "force": true}),
                )
                .await
            {
                eprintln!("cmux placeholder close failed: {error:#}");
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = move_result {
            // Never close a workspace containing any real terminal. A failed
            // move may have succeeded just before an observation timeout.
            if let Ok(surfaces) = self.surface_workspaces().await {
                let real = surfaces.iter().any(|(id, workspace)| {
                    workspace.eq_ignore_ascii_case(destination)
                        && !id.eq_ignore_ascii_case(placeholder)
                });
                if !real {
                    let _ = self
                        .rpc(
                            "workspace.close",
                            json!({"workspace_id": destination, "force": true}),
                        )
                        .await;
                }
            }
            return Err(error);
        }
        Ok(destination.to_owned())
    }

    pub async fn workspace_order(&self) -> Result<Vec<WorkspaceOrder>> {
        let output = tokio::process::Command::new(&self.binary)
            .args(["--json", "--id-format", "uuids", "tree", "--all"])
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(RPC_TIMEOUT, output).await??;
        ensure!(
            output.status.success(),
            "cmux tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let tree: Value = serde_json::from_slice(&output.stdout)?;
        let mut result = Vec::new();
        for window in tree["windows"].as_array().context("missing cmux windows")? {
            let workspaces = window["workspaces"]
                .as_array()
                .context("missing cmux workspaces")?;
            result.push(WorkspaceOrder {
                window: window["id"]
                    .as_str()
                    .context("missing window ID")?
                    .to_owned(),
                ids: workspaces
                    .iter()
                    .map(|w| {
                        w["id"]
                            .as_str()
                            .context("missing workspace ID")
                            .map(str::to_ascii_lowercase)
                    })
                    .collect::<Result<_>>()?,
                pinned: workspaces
                    .iter()
                    .map(|w| w["pinned"].as_bool().unwrap_or(false))
                    .collect(),
            });
        }
        Ok(result)
    }

    pub(super) async fn stack_workspace(&self, workspace: &str) -> Result<()> {
        let workspace = workspace.to_ascii_lowercase();
        for window in self.workspace_order().await? {
            if !window.ids.contains(&workspace) {
                continue;
            }
            let mut desired = vec![workspace.clone()];
            desired.extend(window.ids.iter().filter(|id| **id != workspace).cloned());
            let desired = super::order::project(&window.ids, &desired, &window.pinned);
            if desired != window.ids {
                self.reorder(&window.window, &desired).await?;
            }
        }
        Ok(())
    }

    pub async fn reorder(&self, window: &str, ids: &[String]) -> Result<()> {
        let output = tokio::process::Command::new(&self.binary)
            .args([
                "reorder-workspaces",
                "--window",
                window,
                "--order",
                &ids.join(","),
            ])
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(RPC_TIMEOUT, output).await??;
        ensure!(
            output.status.success(),
            "cmux reorder: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    pub async fn workspace_titles(&self) -> Result<std::collections::HashMap<String, String>> {
        let output = tokio::process::Command::new(&self.binary)
            .args(["--json", "workspace", "list"])
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(RPC_TIMEOUT, output).await??;
        ensure!(
            output.status.success(),
            "cmux workspace list: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout)?;
        let workspaces = response["workspaces"]
            .as_array()
            .context("missing workspace list")?;
        Ok(workspaces
            .iter()
            .filter_map(|item| {
                // Only explicit sidebar names count as user renames. A shell's OSC
                // title updates must not overwrite the conversation title.
                Some((
                    item["id"].as_str()?.to_owned(),
                    item["custom_title"].as_str()?.to_owned(),
                ))
            })
            .collect())
    }

    pub async fn rename(&self, workspace: &str, title: &str) -> Result<()> {
        let output = tokio::process::Command::new(&self.binary)
            .args([
                "workspace",
                "rename",
                workspace,
                &format!("--title={title}"),
            ])
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(RPC_TIMEOUT, output).await??;
        ensure!(
            output.status.success(),
            "cmux rename: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    pub async fn launch(&self, runtime_dir: &Path, launch: &Launch) -> Result<()> {
        // Nothing has been sent to cmux until the workspace RPC below: a failure
        // here is a definite non-start, unlike a failure of the RPC itself.
        let (executable, bootstrap) = (|| -> Result<_> {
            let executable = std::env::current_exe()?;
            let bootstrap = write_zsh_bootstrap(runtime_dir, &executable)?;
            Ok((executable, bootstrap))
        })()
        .map_err(|error| error.context(NotStarted))?;
        let mut argv = vec![
            executable.to_string_lossy().into_owned(),
            "host".into(),
            "--runtime-dir".into(),
            runtime_dir.to_string_lossy().into_owned(),
            "wrap".into(),
            "--session-id".into(),
            launch.id.clone(),
            "--cwd".into(),
            launch.cwd.to_string_lossy().into_owned(),
            "--agent".into(),
            launch.agent.to_string(),
            "--".into(),
        ];
        argv.extend(launch.command.clone());
        // initial_command replaces the terminal's process, rather than typing into
        // an arbitrary focused shell. Every argument is quoted as a POSIX word.
        let mut initial = vec!["env".into(), "CONVERSATIONS_CAPTURE=0".into()];
        // Shell startup can prompt (for example AWS SSO). Start the wrapper
        // first so that prompt is visible and writable from the browser.
        if launch.agent == conversations_core::AgentKind::Zsh {
            initial.extend(argv);
        } else {
            initial.extend(login_command(&argv));
        }
        let command = initial
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let created = self.create_workspace(
            json!({
                "title": launch.title(),
                "working_directory": launch.cwd,
                "initial_command": command,
                // The explicit wrapper command runs after the login shell starts.
                // Auto-wrapping that shell first makes the explicit wrapper exit as nested.
                "workspace_env": {"GHOSTTY_ZSH_ZDOTDIR": bootstrap, "CONVERSATIONS_CAPTURE": "1", "CONVERSATIONS_HOST_DIR": runtime_dir},
                "focus": false,
            }),
        )
        .await?;
        let workspace = created["workspace_id"]
            .as_str()
            .context("missing new workspace ID")?;
        // The shell already exists: do not report launch failure and invite a duplicate.
        if let Err(error) = self.stack_workspace(workspace).await {
            eprintln!("cmux new workspace placement failed: {error:#}");
        }
        Ok(())
    }

    pub async fn start_host(&self, runtime_dir: &Path, server: &str) -> Result<()> {
        // The binding survives cmux snapshots even when no terminal process is
        // running. Never identify a host by its user-editable workspace title.
        let checkpoint = host_checkpoint(runtime_dir);
        for (surface, workspace) in self.surface_workspaces().await? {
            if self.resume_binding(&surface).await?.is_some_and(|binding| {
                binding["kind"] == "conversations" && binding["checkpoint_id"] == checkpoint
            }) {
                return self.start_saved_surface(&workspace, &surface).await;
            }
        }
        let executable = std::env::current_exe()?;
        // No `--backend cmux`: the new workspace inherits a live CMUX_WORKSPACE_ID
        // and `host run`'s auto-detection resolves it to cmux. Compatibility is
        // preserved via explicit `--backend cmux` on the Run subcommand for any
        // operator (or test) that still wants to force the choice.
        let argv = [
            "env".to_owned(),
            "-u".to_owned(),
            "CONVERSATIONS_WRAPPED".to_owned(),
            executable.to_string_lossy().into_owned(),
            "--server".to_owned(),
            server.to_owned(),
            "host".to_owned(),
            "--runtime-dir".to_owned(),
            runtime_dir.to_string_lossy().into_owned(),
            "run".to_owned(),
        ];
        let command = login_command(&argv)
            .iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" ");
        self.create_workspace(
            json!({
                "title": "Conversations host",
                "working_directory": std::env::current_dir()?,
                "initial_command": command,
                "workspace_env": {"CONVERSATIONS_CAPTURE": "0", "CONVERSATIONS_HOST_DIR": runtime_dir, "CONVERSATIONS_HOST_CONTROL": "1"},
                "focus": false,
            }),
        )
        .await?;
        Ok(())
    }
}

pub(super) fn host_checkpoint(runtime: &Path) -> String {
    format!("host:{}", runtime.display())
}

/// GUI launches do not inherit the interactive PATH (npm, Nix, user launchers).
/// Keep argv separate all the way through the login shell, including empty words.
pub(super) fn login_command(argv: &[String]) -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    login_command_for_shell(&shell, argv)
}

fn login_command_for_shell(shell: &str, argv: &[String]) -> Vec<String> {
    let fish = Path::new(shell)
        .file_name()
        .is_some_and(|name| name == "fish");
    let mut command = vec![
        shell.into(),
        "-ilc".into(),
        if fish { "exec $argv" } else { r#"exec "$@""# }.into(),
    ];
    if !fish {
        command.push("conversations".into());
    }
    command.extend_from_slice(argv);
    command
}

/// cmux owns ZDOTDIR; its bootstrap chains through GHOSTTY_ZSH_ZDOTDIR to this file
/// through its own bootstrap. Restore the user's files before replacing the
/// initial interactive shell, so their startup files run once in the child PTY.
fn write_zsh_bootstrap(runtime_dir: &Path, executable: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = runtime_dir.join("zsh-bootstrap");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)?;
    let restore = match std::env::var("ZDOTDIR").ok() {
        Some(value) => format!("export ZDOTDIR={}\n", shell_quote(&value)),
        None => "unset ZDOTDIR\n".to_owned(),
    };
    let script = format!(
        r#"# Generated for this Conversations runtime only.
{restore}if [[ "${{CONVERSATIONS_CAPTURE:-0}}" == 1 && -o interactive && -z "${{ZSH_EXECUTION_STRING:-}}" && -z "${{CONVERSATIONS_WRAPPED:-}}" && -x {executable} ]]; then
    exec {executable} host --runtime-dir {runtime} wrap -- "${{SHELL:-/bin/zsh}}" -l
fi
if [[ -r "${{ZDOTDIR-$HOME}}/.zshenv" ]]; then
    source -- "${{ZDOTDIR-$HOME}}/.zshenv"
fi
"#,
        executable = shell_quote(&executable.to_string_lossy()),
        runtime = shell_quote(&runtime_dir.to_string_lossy())
    );
    let temporary = dir.join(format!(".zshenv-{}", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, script)?;
    std::fs::rename(&temporary, dir.join(".zshenv"))?;
    Ok(dir)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch_fixture(dir: &Path) -> (Cmux, Launch) {
        use std::os::unix::fs::PermissionsExt;
        let binary = dir.join("cmux");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
root=${0%/*}
case "$*" in
"--json --id-format uuids tree --all") cat "$root/tree.json";;
"rpc workspace.create "*)
    printf '%s\n' "$3" >> "$root/creates"
    owner=$(cat "$root/owner")
    case "$3" in
        *\"window_id\":\""$owner"\"*) ;;
        *) echo 'Socket closed before reply: no target window' >&2; exit 1;;
    esac
    if [ -f "$root/disconnect" ]; then
        echo 'Socket closed before reply: creation outcome unknown' >&2
        exit 1
    fi
    echo '{"workspace_id":"child"}';;
*) echo '{}';;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launch = Launch {
            id: "handoff".into(),
            agent: conversations_core::AgentKind::Claude,
            cwd: dir.to_owned(),
            command: vec!["claude".into(), "investigate restoration".into()],
        };
        (
            Cmux {
                binary,
                host_workspace: "CONTROL".into(),
            },
            launch,
        )
    }

    fn save_window_tree(dir: &Path, owner: Option<&str>) {
        std::fs::write(dir.join("owner"), owner.unwrap_or_default()).unwrap();
        let mut windows = vec![json!({"id":"focused-unrelated", "workspaces":[{"id":"other"}]})];
        if let Some(owner) = owner {
            windows.push(json!({"id":owner, "workspaces":[{"id":"control"}]}));
        }
        std::fs::write(
            dir.join("tree.json"),
            json!({"windows":windows}).to_string(),
        )
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn shell_startup_prompt_runs_after_wrapper_start() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "CONVERSATIONS_STARTUP_PROMPT_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::local_host::cmux::tests::shell_startup_prompt_runs_after_wrapper_start", "--nocapture"])
                .env(CHILD, "1").env("SHELL", "/bin/zsh").output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (cmux, mut launch) = launch_fixture(dir.path());
        save_window_tree(dir.path(), Some("host-window"));
        launch.agent = conversations_core::AgentKind::Zsh;
        launch.command = vec!["/bin/zsh".into(), "-ilc".into(), "exit".into()];
        cmux.launch(dir.path(), &launch).await.unwrap();
        let created: Value = serde_json::from_str(
            std::fs::read_to_string(dir.path().join("creates"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        // Stand in for the wrapper at its executable boundary, without a real host.
        let wrapper = dir.path().join("wrapper");
        std::fs::write(&wrapper, "#!/bin/sh\ntouch \"$HOME/wrapped\"\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.path().join(".zshrc"), "[[ -f $HOME/wrapped ]] || { print 'startup prompt has no wrapper'; exit 42; }\nread 'answer?AWS SSO: '\nprint -r -- $answer > $HOME/answer\n").unwrap();
        let command = created["initial_command"].as_str().unwrap().replace(
            &shell_quote(&std::env::current_exe().unwrap().to_string_lossy()),
            &shell_quote(&wrapper.to_string_lossy()),
        );
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .env("HOME", dir.path())
            .env("ZDOTDIR", dir.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"signed-in\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "startup ran before wrapper: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("answer"))
                .unwrap()
                .trim(),
            "signed-in"
        );
    }

    #[tokio::test]
    async fn launch_uses_current_host_window_even_after_workspace_moves() {
        let dir = tempfile::tempdir().unwrap();
        let (cmux, launch) = launch_fixture(dir.path());
        for owner in ["restored-window", "moved-window"] {
            save_window_tree(dir.path(), Some(owner));
            cmux.launch(dir.path(), &launch).await.unwrap();
        }
        let creates = std::fs::read_to_string(dir.path().join("creates")).unwrap();
        assert_eq!(creates.lines().count(), 2);
    }

    #[tokio::test]
    async fn launch_refuses_missing_host_workspace_before_creating_anything() {
        let dir = tempfile::tempdir().unwrap();
        let (cmux, launch) = launch_fixture(dir.path());
        save_window_tree(dir.path(), None);
        let error = cmux.launch(dir.path(), &launch).await.unwrap_err();
        assert!(error.to_string().contains("host workspace"), "{error:#}");
        assert!(!dir.path().join("creates").exists());
    }

    #[tokio::test]
    async fn launch_does_not_retry_ambiguous_workspace_creation() {
        let dir = tempfile::tempdir().unwrap();
        let (cmux, launch) = launch_fixture(dir.path());
        save_window_tree(dir.path(), Some("host-window"));
        std::fs::write(dir.path().join("disconnect"), "").unwrap();
        let error = cmux.launch(dir.path(), &launch).await.unwrap_err();
        assert!(
            error.to_string().contains("creation outcome unknown"),
            "{error:#}"
        );
        let creates = std::fs::read_to_string(dir.path().join("creates")).unwrap();
        assert_eq!(
            creates.lines().count(),
            1,
            "never duplicate a possibly-created workspace"
        );
    }
    #[test]
    fn login_shell_preserves_empty_and_literal_arguments() {
        let home = tempfile::tempdir().unwrap();
        let mut shells = vec!["/bin/sh".to_owned()];
        if Path::new("/bin/zsh").is_file() {
            shells.push("/bin/zsh".to_owned());
        }
        if let Ok(fish) = std::env::var("TEST_FISH_SHELL") {
            shells.push(fish);
        }
        let argv = [
            "/usr/bin/printf",
            "[%s]",
            "",
            "two words",
            "$(false); 'literal'",
        ]
        .map(str::to_owned);
        for shell in shells {
            let command = login_command_for_shell(&shell, &argv);
            let output = std::process::Command::new(&command[0])
                .args(&command[1..])
                .env("HOME", home.path())
                .env("ZDOTDIR", home.path())
                .env("XDG_CONFIG_HOME", home.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{shell}: {:?}", output);
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                "[][two words][$(false); 'literal']",
                "{shell}"
            );
        }
    }

    #[tokio::test]
    async fn new_workspace_stacks_below_pins_without_shuffling_existing_workspaces() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("cmux");
        let calls = dir.path().join("calls");
        let tree = json!({"windows":[{"id":"window", "workspaces":[
            {"id":"pinned", "pinned":true}, {"id":"host"},
            {"id":"older"}, {"id":"unrelated"}, {"id":"new"}
        ]}]});
        std::fs::write(&binary, format!(
            "#!/bin/sh\necho \"$*\" >> {}\ncase \"$*\" in\n*tree*) echo {};;\n*) echo '{{}}';;\nesac\n",
            shell_quote(calls.to_str().unwrap()), shell_quote(&tree.to_string())
        )).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cmux = Cmux {
            binary,
            host_workspace: "host".into(),
        };
        cmux.stack_workspace("new").await.unwrap();
        let observed = std::fs::read_to_string(calls).unwrap();
        assert!(
            observed.contains(
                "reorder-workspaces --window window --order pinned,new,host,older,unrelated"
            ),
            "{observed}"
        );
    }

    #[tokio::test]
    async fn bootstrap_reuses_bound_dormant_host_without_creating_workspace() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("cmux");
        let calls = dir.path().join("calls");
        let tree = json!({"windows":[{"workspaces":[{"id":"workspace", "panes":[{"surfaces":[{"id":"surface"}]}]}]}]});
        let binding = json!({"resume_binding":{"kind":"conversations", "checkpoint_id":host_checkpoint(dir.path())}});
        std::fs::write(&binary, format!(
            "#!/bin/sh\necho \"$*\" >> {}\ncase \"$*\" in\n*tree*) echo {};;\n*resume*) echo {};;\n*) echo '{{}}';;\nesac\n",
            shell_quote(calls.to_str().unwrap()), shell_quote(&tree.to_string()), shell_quote(&binding.to_string())
        )).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cmux = Cmux {
            binary,
            host_workspace: "control".into(),
        };
        cmux.start_host(dir.path(), "http://localhost:1")
            .await
            .unwrap();
        let observed = std::fs::read_to_string(calls).unwrap();
        assert!(observed.contains("surface.read_text"), "{observed}");
        assert!(!observed.contains("workspace.create"), "{observed}");
    }

    #[tokio::test]
    async fn browser_surface_does_not_block_host_creation() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("cmux");
        let calls = dir.path().join("calls");
        let tree = json!({"windows":[{"id":"window", "workspaces":[{"id":"control", "panes":[{"surfaces":[{"id":"browser", "type":"browser"}]}]}]}]});
        std::fs::write(&binary, format!(
            "#!/bin/sh\necho \"$*\" >> {}\ncase \"$*\" in\n*tree*) echo {};;\n*resume*) exit 1;;\n*) echo '{{}}';;\nesac\n",
            shell_quote(calls.to_str().unwrap()), shell_quote(&tree.to_string())
        )).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cmux = Cmux {
            binary,
            host_workspace: "control".into(),
        };
        cmux.start_host(dir.path(), "http://localhost:1")
            .await
            .unwrap();
        let observed = std::fs::read_to_string(calls).unwrap();
        assert!(observed.contains("workspace.create"), "{observed}");
        assert!(!observed.contains("resume"), "{observed}");
    }

    #[test]
    fn shell_arguments_are_literal_even_with_metacharacters() {
        let value = "a b'c; $(touch SHOULD_NOT_EXIST) `false`\nend";
        let result = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("printf %s {}", shell_quote(value))])
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(String::from_utf8(result.stdout).unwrap(), value);
    }
}
