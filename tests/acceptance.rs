use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pen-{name}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn reply(result: &str) -> String {
    format!(r#"{{"id":"test","result":{result}}}"#)
}

fn mock_herdr(
    socket: &Path,
    replies: Vec<String>,
) -> (Arc<Mutex<Vec<serde_json::Value>>>, thread::JoinHandle<()>) {
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        for response in replies {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock herdr accept failed: {error}"),
                }
            };
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            seen.lock()
                .unwrap()
                .push(serde_json::from_str(line.trim()).unwrap());
            writeln!(stream, "{response}").unwrap();
        }
    });
    (requests, handle)
}

fn pen(temp: &TempDir, socket: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pen"));
    command
        .env("PEN_CONFIG_DIR", temp.0.join("config"))
        .env("PEN_SOCKET", socket);
    command
}

#[test]
fn save_persists_the_current_workspace_layout_as_toml() {
    let temp = TempDir::new("save");
    let socket = temp.0.join("herdr.sock");
    let (requests, server) = mock_herdr(
        &socket,
        vec![
            reply(
                r#"{"type":"pane_current","pane":{"pane_id":"w7:p1","workspace_id":"w7","tab_id":"w7:t1"}}"#,
            ),
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w7","active_tab_id":"w7:t1","label":"demo/project","focused":true}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w7","tab_id":"w7:t1","zoomed":false,"focused_pane_id":"w7:p1","root":{"type":"split","direction":"right","ratio":0.6,"first":{"type":"pane","pane_id":"w7:p1","cwd":"/work","command":["codex"],"env":{"MODE":"dev"},"label":"agent"},"second":{"type":"pane","pane_id":"w7:p2","cwd":"/work/docs","command":["zsh"]}}}}"#,
            ),
            reply(r#"{"type":"notification_show"}"#),
        ],
    );

    let output = pen(&temp, &socket).arg("save").output().unwrap();
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved = fs::read_to_string(temp.0.join("config/demo_project.toml")).unwrap();
    assert!(saved.contains("label = \"demo/project\""));
    assert!(saved.contains("direction = \"right\""));
    assert!(saved.contains("command = [\"codex\"]"));
    assert!(saved.contains("MODE = \"dev\""));
    assert!(!saved.contains("pane_id"));
    let methods: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        methods,
        [
            "pane.current",
            "workspace.list",
            "layout.export",
            "notification.show"
        ]
    );
    assert_eq!(requests.lock().unwrap()[2]["params"]["tab_id"], "w7:t1");
    assert_eq!(
        requests.lock().unwrap()[3]["params"]["title"],
        "セッション demo/project を保存しました"
    );
}

#[test]
fn save_succeeds_and_warns_when_the_toast_cannot_be_shown() {
    let temp = TempDir::new("save-toast-failure");
    let socket = temp.0.join("herdr.sock");
    let (requests, server) = mock_herdr(
        &socket,
        vec![
            reply(
                r#"{"type":"pane_current","pane":{"pane_id":"w7:p1","workspace_id":"w7","tab_id":"w7:t1"}}"#,
            ),
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w7","active_tab_id":"w7:t1","label":"demo/project","focused":true}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w7","tab_id":"w7:t1","zoomed":false,"focused_pane_id":"w7:p1","root":{"type":"pane","pane_id":"w7:p1","cwd":"/work"}}}"#,
            ),
            r#"{"id":"test","error":{"message":"notifications unavailable"}}"#.to_owned(),
        ],
    );

    let output = pen(&temp, &socket).arg("save").output().unwrap();
    server.join().unwrap();

    // 保存データの成否と補助通知の成否は分離する: toast 失敗でも save は成功
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved = fs::read_to_string(temp.0.join("config/demo_project.toml")).unwrap();
    assert!(saved.contains("label = \"demo/project\""));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pen: notification failed:")
            && stderr.contains("notifications unavailable"),
        "stderr should warn about the failed toast: {stderr}"
    );
    let methods: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        methods,
        [
            "pane.current",
            "workspace.list",
            "layout.export",
            "notification.show"
        ]
    );
}

#[test]
fn close_keeps_the_saved_definition_and_closes_the_current_workspace() {
    let temp = TempDir::new("close");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("demo.toml"),
        "label = \"demo\"\n\n[root]\ntype = \"pane\"\ncwd = \"/work\"\n",
    )
    .unwrap();
    let socket = temp.0.join("herdr.sock");
    let (requests, server) = mock_herdr(
        &socket,
        vec![
            reply(
                r#"{"type":"pane_current","pane":{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1"}}"#,
            ),
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w2","active_tab_id":"w2:t1","label":"demo","focused":true}]}"#,
            ),
            reply(r#"{"type":"workspace_closed","workspace_id":"w2"}"#),
        ],
    );

    let output = pen(&temp, &socket).arg("close").output().unwrap();
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(config.join("demo.toml").exists());
    let seen = requests.lock().unwrap();
    assert_eq!(seen[2]["method"], "workspace.close");
    assert_eq!(seen[2]["params"]["workspace_id"], "w2");
}

#[test]
fn picker_space_restores_a_saved_workspace() {
    let temp = TempDir::new("picker");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("sleeping.toml"),
        "label = \"sleeping\"\n\n[root]\ntype = \"pane\"\ncwd = \"/sleeping\"\ncommand = [\"claude\"]\n",
    )
    .unwrap();
    let fake_fzf = temp.0.join("fzf");
    fs::write(&fake_fzf, "#!/bin/sh\nprintf 'space\\n○\\tsleeping\\n'\n").unwrap();
    fs::set_permissions(&fake_fzf, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = temp.0.join("herdr.sock");
    let (requests, server) = mock_herdr(
        &socket,
        vec![
            reply(r#"{"type":"workspace_list","workspaces":[]}"#),
            reply(
                r#"{"type":"layout_apply","layout":{"workspace_id":"w9","tab_id":"w9:t1","zoomed":false,"focused_pane_id":"w9:p1","root":{"type":"pane","pane_id":"w9:p1","cwd":"/sleeping","command":["claude"]}}}"#,
            ),
        ],
    );

    let output = pen(&temp, &socket)
        .env("PEN_FZF", &fake_fzf)
        .arg("picker")
        .output()
        .unwrap();
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen = requests.lock().unwrap();
    assert_eq!(seen[1]["method"], "layout.apply");
    assert_eq!(seen[1]["params"]["focus"], true);
    assert_eq!(seen[1]["params"]["tab_label"], "sleeping");
    assert_eq!(seen[1]["params"]["root"]["command"][0], "claude");
}

#[test]
fn version_flag_reports_the_cargo_package_version() {
    for flag in ["--version", "-V", "version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pen"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("pen {}\n", env!("CARGO_PKG_VERSION")),
            "{flag}"
        );
    }
}
