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

const HELP: &str = "pen — suspend and restore herdr workspaces\n\nUsage:\n  pen save\n  pen close\n  pen picker\n  pen --version\n";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Definition {
    label: String,
    tabs: Vec<TabDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TabDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    active: bool,
    root: LayoutNode,
}

/// 保存ファイルの直列化形。v0.1.x は単一 [root] だったので、既存ファイルは
/// legacy として読み、1 tab の Definition へ正規化する。書き込みは常に新形式。
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredDefinition {
    Tabs(Definition),
    Legacy { label: String, root: LayoutNode },
}

impl From<StoredDefinition> for Definition {
    fn from(stored: StoredDefinition) -> Self {
        match stored {
            StoredDefinition::Tabs(definition) => definition,
            StoredDefinition::Legacy { label, root } => Self {
                label,
                tabs: vec![TabDefinition {
                    label: None,
                    active: true,
                    root,
                }],
            },
        }
    }
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
        // layout.export が返す実 pane の参照。command 捕捉にだけ使い、保存はしない
        #[serde(default, skip_serializing)]
        pane_id: Option<String>,
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
    // active tab の判定は workspace.list のこの値で行う。TabInfo.focused は
    // focus されていない workspace では全 tab false になり判定に使えない (実測)
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
struct CreatedTab {
    tab_id: String,
}

#[derive(Deserialize)]
struct CreatedWorkspace {
    workspace_id: String,
}

#[derive(Deserialize)]
struct WorkspaceCreateResult {
    workspace: CreatedWorkspace,
    tab: CreatedTab,
}

#[derive(Deserialize)]
struct TabRow {
    tab_id: String,
    label: String,
}

#[derive(Deserialize)]
struct TabListResult {
    tabs: Vec<TabRow>,
}

#[derive(Deserialize)]
struct ForegroundProcess {
    pid: u64,
    argv: Vec<String>,
}

#[derive(Deserialize)]
struct ProcessInfo {
    shell_pid: u64,
    foreground_process_group_id: u64,
    foreground_processes: Vec<ForegroundProcess>,
}

#[derive(Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

#[derive(Deserialize)]
struct AppliedLayout {
    tab_id: String,
}

#[derive(Deserialize)]
struct LayoutApplyResult {
    layout: AppliedLayout,
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
        // pane.current は caller_pane_id がないと「focus 中の pane」に解決される。
        // herdr が pane の shell に配る HERDR_PANE_ID を渡し、focus がどこに
        // あっても呼び出し元自身の workspace を対象にする。
        let params = match env::var("HERDR_PANE_ID") {
            Ok(pane_id) => json!({ "caller_pane_id": pane_id }),
            Err(_) => json!({}),
        };
        Ok(self
            .call::<PaneCurrentResult>("pane.current", &params)?
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

    fn tabs(&self, workspace_id: &str) -> Result<Vec<TabRow>> {
        Ok(self
            .call::<TabListResult>("tab.list", &json!({ "workspace_id": workspace_id }))?
            .tabs)
    }

    fn pane_command(&self, pane_id: &str) -> Result<Option<Vec<String>>> {
        let info = self
            .call::<ProcessInfoResult>("pane.process_info", &json!({ "pane_id": pane_id }))?
            .process_info;
        Ok(command_from_process_info(
            &info,
            u64::from(std::process::id()),
        ))
    }

    fn focus_tab(&self, tab_id: &str) -> Result<()> {
        let _: Value = self.call("tab.focus", &json!({ "tab_id": tab_id }))?;
        Ok(())
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

    fn restore(&self, definition: &Definition) -> Result<()> {
        let Some((first, rest)) = definition.tabs.split_first() else {
            return Err(format!("definition {:?} has no tabs", definition.label).into());
        };
        // layout.apply は workspace を作らない (workspace 指定なしだと呼び出し元
        // workspace への新規 tab になる) ので、先に復元先 workspace を用意する。
        let created: WorkspaceCreateResult = self.call(
            "workspace.create",
            &json!({ "label": definition.label, "focus": false }),
        )?;
        let hint = |error: Box<dyn Error>| {
            format!(
                "{error} (restore left an incomplete workspace {:?}; close it manually)",
                definition.label
            )
        };
        // tab_id と workspace_id の併用は herdr が invalid_target で拒否する。
        // 初期 tab の置換を最初に行う: 兄弟 tab がいる状態で置換するとその tab は
        // 末尾へ移動するため、置換 → append の順でだけ保存順が保たれる (実測)。
        let applied: LayoutApplyResult = self
            .call(
                "layout.apply",
                &json!({
                    "root": first.root,
                    "tab_id": created.tab.tab_id,
                    "tab_label": first.label,
                    "focus": false,
                }),
            )
            .map_err(hint)?;
        let mut restored_tab_ids = vec![applied.layout.tab_id];
        for tab in rest {
            let applied: LayoutApplyResult = self
                .call(
                    "layout.apply",
                    &json!({
                        "root": tab.root,
                        "workspace_id": created.workspace.workspace_id,
                        "tab_label": tab.label,
                        "focus": false,
                    }),
                )
                .map_err(hint)?;
            restored_tab_ids.push(applied.layout.tab_id);
        }
        // 全 tab が揃ってから、保存時に active だった tab を選んで前面へ
        let active = definition
            .tabs
            .iter()
            .position(|tab| tab.active)
            .unwrap_or(0);
        self.focus_tab(&restored_tab_ids[active]).map_err(hint)?;
        self.focus(&created.workspace.workspace_id).map_err(hint)?;
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
        Some("version" | "--version" | "-V") => {
            println!("pen {VERSION}");
            Ok(())
        }
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
    save_definition(config, &snapshot_definition(herdr, workspace)?)?;
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
    save_definition(config, &snapshot_definition(herdr, workspace)?)
}

/// workspace の全 tab を export し、pane ごとの前景コマンドを添えた定義を作る。
fn snapshot_definition(herdr: &Herdr, workspace: &Workspace) -> Result<Definition> {
    let mut tabs = Vec::new();
    for tab in herdr.tabs(&workspace.id)? {
        let mut root = herdr.export_tab(&tab.tab_id)?;
        capture_commands(herdr, &mut root);
        tabs.push(TabDefinition {
            active: tab.tab_id == workspace.active_tab_id,
            label: Some(tab.label),
            root,
        });
    }
    if tabs.is_empty() {
        return Err(format!("workspace {} has no tabs", workspace.label).into());
    }
    Ok(Definition {
        label: workspace.label.clone(),
        tabs,
    })
}

/// pane の前景コマンドを決める。復元して意味があるのは前景 process group の
/// leader だけで、次は保存しない: shell 自身 (素の shell は素の shell に戻る)、
/// `pen` 自身 (対話実行中の save では pen が前景 leader になる)、argv が
/// 取れなかった process、leader 以外の子 process。
fn command_from_process_info(info: &ProcessInfo, self_pid: u64) -> Option<Vec<String>> {
    if info.foreground_process_group_id == info.shell_pid
        || info.foreground_process_group_id == self_pid
    {
        return None;
    }
    info.foreground_processes
        .iter()
        .find(|process| process.pid == info.foreground_process_group_id)
        .map(|process| process.argv.clone())
        .filter(|argv| !argv.is_empty())
}

/// `layout.export` は実行中コマンドを含まないので、`pane.process_info` で
/// 前景コマンドを補う。取得失敗は警告に留め、layout だけでも保存する。
fn capture_commands(herdr: &Herdr, node: &mut LayoutNode) {
    match node {
        LayoutNode::Pane {
            command, pane_id, ..
        } => {
            let Some(pane_id) = pane_id else { return };
            match herdr.pane_command(pane_id) {
                Ok(Some(argv)) => *command = Some(argv),
                Ok(None) => {}
                Err(error) => eprintln!("pen: cannot inspect pane {pane_id}: {error}"),
            }
        }
        LayoutNode::Split { first, second, .. } => {
            capture_commands(herdr, first);
            capture_commands(herdr, second);
        }
    }
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
            herdr.restore(definition)?;
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
        let stored: StoredDefinition = toml::from_str(&fs::read_to_string(&path)?)
            .map_err(|error| format!("invalid definition {}: {error}", path.display()))?;
        let definition = Definition::from(stored);
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
    use super::{ForegroundProcess, ProcessInfo, command_from_process_info, safe_filename};

    #[test]
    fn unsafe_label_characters_are_normalized() {
        assert_eq!(safe_filename("demo/project:*?"), "demo_project___");
        assert_eq!(safe_filename("..."), "workspace");
    }

    fn info(shell_pid: u64, leader: u64, processes: &[(u64, &[&str])]) -> ProcessInfo {
        ProcessInfo {
            shell_pid,
            foreground_process_group_id: leader,
            foreground_processes: processes
                .iter()
                .map(|(pid, argv)| ForegroundProcess {
                    pid: *pid,
                    argv: argv.iter().map(ToString::to_string).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn foreground_leader_argv_becomes_the_saved_command() {
        // leader の argv を保存し、子 process (uv 等) は選ばない
        let info = info(100, 200, &[(200, &["claude"]), (201, &["uv", "tool"])]);
        assert_eq!(
            command_from_process_info(&info, 999),
            Some(vec!["claude".to_owned()])
        );
    }

    #[test]
    fn idle_shell_and_pen_itself_are_not_saved_as_commands() {
        // 素の shell: 前景 group が shell 自身
        let idle = info(100, 100, &[(100, &["/usr/bin/zsh"])]);
        assert_eq!(command_from_process_info(&idle, 999), None);
        // 対話実行中の save では pen 自身が前景 leader になる
        let running_pen = info(100, 555, &[(555, &["pen", "save"])]);
        assert_eq!(command_from_process_info(&running_pen, 555), None);
        // leader の argv が取れない場合は command を書かない
        let no_argv = info(100, 200, &[(200, &[])]);
        assert_eq!(command_from_process_info(&no_argv, 999), None);
    }
}
