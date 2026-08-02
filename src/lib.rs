use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const HELP: &str =
    "pen — suspend and restore herdr workspaces\n\nUsage:\n  pen save\n  pen close\n  pen picker\n";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Definition {
    label: String,
    root: LayoutNode,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum LayoutNode {
    Pane {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Split {
        direction: String,
        ratio: f64,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Clone, Debug, Deserialize)]
struct Workspace {
    #[serde(rename = "workspace_id")]
    id: String,
    active_tab_id: String,
    label: String,
}

#[derive(Deserialize)]
struct PaneInfo {
    workspace_id: String,
}

#[derive(Deserialize)]
struct PaneCurrentResult {
    pane: PaneInfo,
}

#[derive(Deserialize)]
struct WorkspaceListResult {
    workspaces: Vec<Workspace>,
}

#[derive(Deserialize)]
struct LayoutExportResult {
    layout: ExportedLayout,
}

#[derive(Deserialize)]
struct ExportedLayout {
    root: LayoutNode,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

struct Herdr {
    socket: PathBuf,
}

impl Herdr {
    fn from_env() -> Result<Self> {
        let socket = match env::var_os("PEN_SOCKET") {
            Some(path) => PathBuf::from(path),
            None => home_dir()?.join(".config/herdr/herdr.sock"),
        };
        Ok(Self { socket })
    }

    fn call<T: DeserializeOwned>(&self, method: &str, params: &Value) -> Result<T> {
        let mut stream = UnixStream::connect(&self.socket).map_err(|error| {
            format!(
                "cannot connect to herdr at {}: {error}",
                self.socket.display()
            )
        })?;
        let request = json!({
            "id": format!("pen:{method}"),
            "method": method,
            "params": params,
        });
        serde_json::to_writer(&mut stream, &request)?;
        stream.write_all(b"\n")?;

        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response)?;
        if response.is_empty() {
            return Err(format!("herdr returned no response for {method}").into());
        }
        let envelope: Envelope = serde_json::from_str(&response)?;
        if let Some(error) = envelope.error {
            return Err(format!("herdr {method} failed: {error}").into());
        }
        let result = envelope
            .result
            .ok_or_else(|| format!("herdr {method} response has no result"))?;
        Ok(serde_json::from_value(result)?)
    }

    fn current_pane(&self) -> Result<PaneInfo> {
        Ok(self
            .call::<PaneCurrentResult>("pane.current", &json!({}))?
            .pane)
    }

    fn workspaces(&self) -> Result<Vec<Workspace>> {
        Ok(self
            .call::<WorkspaceListResult>("workspace.list", &json!({}))?
            .workspaces)
    }

    fn export_tab(&self, tab_id: &str) -> Result<LayoutNode> {
        Ok(self
            .call::<LayoutExportResult>("layout.export", &json!({ "tab_id": tab_id }))?
            .layout
            .root)
    }

    fn close(&self, workspace_id: &str) -> Result<()> {
        let _: Value = self.call("workspace.close", &json!({ "workspace_id": workspace_id }))?;
        Ok(())
    }

    fn notify(&self, title: &str) -> Result<()> {
        let _: Value = self.call("notification.show", &json!({ "title": title }))?;
        Ok(())
    }

    fn focus(&self, workspace_id: &str) -> Result<()> {
        let _: Value = self.call("workspace.focus", &json!({ "workspace_id": workspace_id }))?;
        Ok(())
    }

    fn apply(&self, definition: &Definition) -> Result<()> {
        let _: Value = self.call(
            "layout.apply",
            &json!({
                "root": definition.root,
                "tab_label": definition.label,
                "focus": true,
            }),
        )?;
        Ok(())
    }
}

/// Runs the requested `pen` subcommand.
///
/// # Errors
///
/// Returns an error when arguments are invalid, herdr cannot be reached, or
/// workspace definitions cannot be read or written.
pub fn run<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let command = args.next();
    if args.next().is_some() {
        return Err(format!("too many arguments\n\n{HELP}").into());
    }

    match command.as_deref() {
        Some("save") => save_current(&Herdr::from_env()?, &config_dir()?),
        Some("close") => close_current(&Herdr::from_env()?, &config_dir()?),
        Some("picker") => picker(&Herdr::from_env()?, &config_dir()?),
        Some("help" | "--help" | "-h") | None => {
            print!("{HELP}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command: {other}\n\n{HELP}").into()),
    }
}

fn save_current(herdr: &Herdr, config: &Path) -> Result<()> {
    let pane = herdr.current_pane()?;
    let workspaces = herdr.workspaces()?;
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.id == pane.workspace_id)
        .ok_or_else(|| format!("current workspace {} was not found", pane.workspace_id))?;
    let root = herdr.export_tab(&workspace.active_tab_id)?;
    save_definition(
        config,
        &Definition {
            label: workspace.label.clone(),
            root,
        },
    )?;
    // 保存は完了している。toast は通知でしかないので失敗しても save を汚さない
    if let Err(error) = herdr.notify(&format!("セッション {} を保存しました", workspace.label))
    {
        eprintln!("pen: notification failed: {error}");
    }
    println!("saved {}", workspace.label);
    Ok(())
}

fn close_current(herdr: &Herdr, config: &Path) -> Result<()> {
    let pane = herdr.current_pane()?;
    let workspaces = herdr.workspaces()?;
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.id == pane.workspace_id)
        .ok_or_else(|| format!("current workspace {} was not found", pane.workspace_id))?;

    ensure_saved(herdr, config, workspace)?;
    herdr.close(&workspace.id)?;
    println!("closed {}", workspace.label);
    Ok(())
}

fn ensure_saved(herdr: &Herdr, config: &Path, workspace: &Workspace) -> Result<()> {
    if find_definition(config, &workspace.label)?.is_some() {
        return Ok(());
    }
    if !confirm(&format!(
        "Save workspace {:?} before closing? [y/N] ",
        workspace.label
    ))? {
        return Err("close cancelled; workspace has no saved definition".into());
    }
    let root = herdr.export_tab(&workspace.active_tab_id)?;
    save_definition(
        config,
        &Definition {
            label: workspace.label.clone(),
            root,
        },
    )
}

fn picker(herdr: &Herdr, config: &Path) -> Result<()> {
    let definitions = load_definitions(config)?;
    let workspaces = herdr.workspaces()?;
    let mut rows = BTreeMap::<String, bool>::new();
    for label in definitions.keys() {
        rows.insert(label.clone(), false);
    }
    for workspace in &workspaces {
        rows.insert(workspace.label.clone(), true);
    }
    if rows.is_empty() {
        return Err("no saved or running workspaces".into());
    }

    let input = rows
        .iter()
        .map(|(label, active)| format!("{}\t{label}", if *active { "●" } else { "○" }))
        .collect::<Vec<_>>()
        .join("\n");
    let fzf = env::var_os("PEN_FZF").unwrap_or_else(|| "fzf".into());
    let mut child = Command::new(fzf)
        .args([
            "--ansi",
            "--no-sort",
            "--prompt=pen> ",
            "--expect=enter,space,esc",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start fzf: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "{input}");
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8(output.stdout)?;
    let mut lines = text.lines();
    let key = lines.next().unwrap_or_default();
    let row = lines.next().unwrap_or_default();
    let label = row
        .split_once('\t')
        .map(|(_, label)| label)
        .filter(|label| !label.is_empty());
    let Some(label) = label else {
        return Ok(());
    };
    let active = workspaces.iter().find(|workspace| workspace.label == label);

    match (key, active) {
        ("space", Some(workspace)) => {
            ensure_saved(herdr, config, workspace)?;
            herdr.close(&workspace.id)?;
            println!("closed {label}");
        }
        ("space" | "enter", None) => {
            let definition = definitions
                .get(label)
                .ok_or_else(|| format!("saved definition not found: {label}"))?;
            herdr.apply(definition)?;
            println!("restored {label}");
        }
        ("enter", Some(workspace)) => {
            herdr.focus(&workspace.id)?;
        }
        _ => {}
    }
    Ok(())
}

fn save_definition(config: &Path, definition: &Definition) -> Result<()> {
    fs::create_dir_all(config)?;
    let path = config.join(format!("{}.toml", safe_filename(&definition.label)));
    fs::write(path, toml::to_string_pretty(definition)?)?;
    Ok(())
}

fn find_definition(config: &Path, label: &str) -> Result<Option<Definition>> {
    Ok(load_definitions(config)?.remove(label))
}

fn load_definitions(config: &Path) -> Result<BTreeMap<String, Definition>> {
    let mut definitions = BTreeMap::new();
    if !config.exists() {
        return Ok(definitions);
    }
    for entry in fs::read_dir(config)? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        let definition: Definition = toml::from_str(&fs::read_to_string(&path)?)
            .map_err(|error| format!("invalid definition {}: {error}", path.display()))?;
        definitions.insert(definition.label.clone(), definition);
    }
    Ok(definitions)
}

fn safe_filename(label: &str) -> String {
    let name: String = label
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect();
    let name = name.trim().trim_matches('.');
    if name.is_empty() {
        "workspace".to_owned()
    } else {
        name.to_owned()
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn config_dir() -> Result<PathBuf> {
    Ok(match env::var_os("PEN_CONFIG_DIR") {
        Some(path) => PathBuf::from(path),
        None => home_dir()?.join(".config/pen"),
    })
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

#[cfg(test)]
mod tests {
    use super::safe_filename;

    #[test]
    fn unsafe_label_characters_are_normalized() {
        assert_eq!(safe_filename("demo/project:*?"), "demo_project___");
        assert_eq!(safe_filename("..."), "workspace");
    }
}
