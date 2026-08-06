use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(_name: &str) -> Self {
        loop {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            // macOS limits Unix socket paths to 104 bytes. Its temp_dir() can
            // already be long, so keep the test socket rooted at short /tmp.
            let path = PathBuf::from("/tmp").join(format!("p{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("cannot create test directory {}: {error}", path.display()),
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn parallel_test_sockets_are_unique_and_short_enough_for_macos() {
    const COUNT: usize = 64;
    let barrier = Arc::new(Barrier::new(COUNT));
    let handles = (0..COUNT)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let temp = TempDir::new("parallel");
                let socket = temp.0.join("herdr.sock");
                let listener = UnixListener::bind(&socket).unwrap();
                (temp, socket, listener)
            })
        })
        .collect::<Vec<_>>();
    let sockets = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();

    let mut paths = sockets.iter().map(|(_, path, _)| path).collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), COUNT);
    assert!(paths.iter().all(|path| path.as_os_str().len() < 104));
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
        .env("PEN_SOCKET", socket)
        // 開発環境の herdr pane から test が走っても結果が変わらないよう固定する
        .env("HERDR_PANE_ID", "w7:p1");
    command
}

#[test]
fn save_persists_every_tab_with_foreground_commands_as_toml() {
    let temp = TempDir::new("save");
    let socket = temp.0.join("herdr.sock");
    // 実 herdr (protocol 17) の意味論: layout.export は実行中コマンドを返さない。
    // 前景コマンドは pane.process_info で pane ごとに捕捉する。
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
                r#"{"type":"tab_list","tabs":[{"tab_id":"w7:t1","workspace_id":"w7","number":1,"label":"agent","focused":true,"pane_count":2,"agent_status":"working"},{"tab_id":"w7:t2","workspace_id":"w7","number":2,"label":"docs","focused":false,"pane_count":1,"agent_status":"unknown"}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w7","tab_id":"w7:t1","zoomed":false,"focused_pane_id":"w7:p1","root":{"type":"split","direction":"right","ratio":0.6,"first":{"type":"pane","pane_id":"w7:p1","cwd":"/work","env":{"MODE":"dev"},"label":"agent"},"second":{"type":"pane","pane_id":"w7:p2","cwd":"/work/docs"}}}}"#,
            ),
            reply(
                r#"{"type":"pane_process_info","process_info":{"pane_id":"w7:p1","shell_pid":100,"foreground_process_group_id":200,"foreground_processes":[{"pid":200,"name":"claude","argv":["claude"],"cmdline":"claude","cwd":"/work"},{"pid":201,"name":"uv","argv":["uv","tool"],"cmdline":"uv tool","cwd":"/work"}]}}"#,
            ),
            reply(
                r#"{"type":"pane_process_info","process_info":{"pane_id":"w7:p2","shell_pid":300,"foreground_process_group_id":300,"foreground_processes":[{"pid":300,"name":"zsh","argv":["/usr/bin/zsh"],"cmdline":"/usr/bin/zsh","cwd":"/work/docs"}]}}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w7","tab_id":"w7:t2","zoomed":false,"focused_pane_id":"w7:p3","root":{"type":"pane","pane_id":"w7:p3","cwd":"/work"}}}"#,
            ),
            reply(
                r#"{"type":"pane_process_info","process_info":{"pane_id":"w7:p3","shell_pid":400,"foreground_process_group_id":500,"foreground_processes":[{"pid":500,"name":"codex","argv":["codex"],"cmdline":"codex","cwd":"/work"}]}}"#,
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
    assert_eq!(saved.matches("[[tabs]]").count(), 2);
    assert!(saved.contains("label = \"agent\""));
    assert!(saved.contains("label = \"docs\""));
    // 保存時の active tab (workspace.active_tab_id = w7:t1 = agent) だけに記す
    assert_eq!(saved.matches("active = true").count(), 1);
    assert!(saved.find("active = true").unwrap() < saved.find("label = \"docs\"").unwrap());
    assert!(saved.contains("direction = \"right\""));
    // 前景コマンドが pane に載る。素の shell (w7:p2) には command を書かない
    assert!(saved.contains("command = [\"claude\"]"));
    assert!(saved.contains("command = [\"codex\"]"));
    assert_eq!(saved.matches("command = ").count(), 2);
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
            "tab.list",
            "layout.export",
            "pane.process_info",
            "pane.process_info",
            "layout.export",
            "pane.process_info",
            "notification.show"
        ]
    );
    let seen = requests.lock().unwrap();
    // focus がどこにあっても呼び出し元の workspace を保存する
    assert_eq!(seen[0]["params"]["caller_pane_id"], "w7:p1");
    assert_eq!(seen[2]["params"]["workspace_id"], "w7");
    assert_eq!(seen[3]["params"]["tab_id"], "w7:t1");
    assert_eq!(seen[4]["params"]["pane_id"], "w7:p1");
    assert_eq!(seen[5]["params"]["pane_id"], "w7:p2");
    assert_eq!(seen[6]["params"]["tab_id"], "w7:t2");
    assert_eq!(seen[7]["params"]["pane_id"], "w7:p3");
    assert_eq!(
        seen[8]["params"]["title"],
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
                r#"{"type":"tab_list","tabs":[{"tab_id":"w7:t1","workspace_id":"w7","number":1,"label":"1","focused":true,"pane_count":1,"agent_status":"unknown"}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w7","tab_id":"w7:t1","zoomed":false,"focused_pane_id":"w7:p1","root":{"type":"pane","pane_id":"w7:p1","cwd":"/work"}}}"#,
            ),
            reply(
                r#"{"type":"pane_process_info","process_info":{"pane_id":"w7:p1","shell_pid":100,"foreground_process_group_id":100,"foreground_processes":[{"pid":100,"name":"zsh","argv":["/usr/bin/zsh"],"cmdline":"/usr/bin/zsh","cwd":"/work"}]}}"#,
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
            "tab.list",
            "layout.export",
            "pane.process_info",
            "notification.show"
        ]
    );
}

#[test]
fn save_fails_and_keeps_the_old_definition_when_a_pane_cannot_be_inspected() {
    let temp = TempDir::new("save-inspect-failure");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    // command は保存内容そのもの: 捕捉に失敗した不完全な snapshot で
    // 既存の定義を上書きしてはならない
    let original = "label = \"demo/project\"\n\n[root]\ntype = \"pane\"\ncwd = \"/old\"\n";
    fs::write(config.join("demo_project.toml"), original).unwrap();
    let socket = temp.0.join("herdr.sock");
    let (_requests, server) = mock_herdr(
        &socket,
        vec![
            reply(
                r#"{"type":"pane_current","pane":{"pane_id":"w7:p1","workspace_id":"w7","tab_id":"w7:t1"}}"#,
            ),
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w7","active_tab_id":"w7:t1","label":"demo/project","focused":true}]}"#,
            ),
            reply(
                r#"{"type":"tab_list","tabs":[{"tab_id":"w7:t1","workspace_id":"w7","number":1,"label":"1","focused":true,"pane_count":1,"agent_status":"unknown"}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w7","tab_id":"w7:t1","zoomed":false,"focused_pane_id":"w7:p1","root":{"type":"pane","pane_id":"w7:p1","cwd":"/work"}}}"#,
            ),
            r#"{"id":"test","error":{"code":"internal","message":"process scan failed"}}"#
                .to_owned(),
        ],
    );

    let output = pen(&temp, &socket).arg("save").output().unwrap();
    server.join().unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pane.process_info") && stderr.contains("process scan failed"),
        "stderr should name the failed call: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(config.join("demo_project.toml")).unwrap(),
        original
    );
}

#[test]
fn save_fails_when_a_foreground_command_cannot_be_determined() {
    let temp = TempDir::new("save-no-argv");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    // 応答としては valid だが、前景 leader (pid 200) の argv が null。
    // 「実行中の何かがある」と分かっているのに command を特定できない snapshot は
    // 素の shell と同じ成功にせず、既存の完全な定義を守る
    let original = "label = \"demo/project\"\n\n[root]\ntype = \"pane\"\ncwd = \"/old\"\n";
    fs::write(config.join("demo_project.toml"), original).unwrap();
    let socket = temp.0.join("herdr.sock");
    let (_requests, server) = mock_herdr(
        &socket,
        vec![
            reply(
                r#"{"type":"pane_current","pane":{"pane_id":"w7:p1","workspace_id":"w7","tab_id":"w7:t1"}}"#,
            ),
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w7","active_tab_id":"w7:t1","label":"demo/project","focused":true}]}"#,
            ),
            reply(
                r#"{"type":"tab_list","tabs":[{"tab_id":"w7:t1","workspace_id":"w7","number":1,"label":"1","focused":true,"pane_count":1,"agent_status":"unknown"}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w7","tab_id":"w7:t1","zoomed":false,"focused_pane_id":"w7:p1","root":{"type":"pane","pane_id":"w7:p1","cwd":"/work"}}}"#,
            ),
            reply(
                r#"{"type":"pane_process_info","process_info":{"pane_id":"w7:p1","shell_pid":100,"foreground_process_group_id":200,"foreground_processes":[{"pid":200,"name":"claude","argv":null,"cmdline":null,"cwd":"/work"}]}}"#,
            ),
        ],
    );

    let output = pen(&temp, &socket).arg("save").output().unwrap();
    server.join().unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("w7:p1") && stderr.contains("no argv"),
        "stderr should name the pane and the missing argv: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(config.join("demo_project.toml")).unwrap(),
        original
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
fn picker_space_restores_a_legacy_single_root_definition() {
    let temp = TempDir::new("picker");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    // v0.1.x の旧形式 (単一 [root]) も 1 tab の workspace として復元できること
    fs::write(
        config.join("sleeping.toml"),
        "label = \"sleeping\"\n\n[root]\ntype = \"pane\"\ncwd = \"/sleeping\"\ncommand = [\"claude\"]\n",
    )
    .unwrap();
    let fake_fzf = temp.0.join("fzf");
    fs::write(&fake_fzf, "#!/bin/sh\nprintf 'space\\n○\\tsleeping\\n'\n").unwrap();
    fs::set_permissions(&fake_fzf, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = temp.0.join("herdr.sock");
    // 復元先 workspace は pen が workspace.create で作る。layout.apply 単発では
    // 呼び出し元 workspace へ tab が刺さるだけ、が実 herdr (protocol 17) の意味論。
    let (requests, server) = mock_herdr(
        &socket,
        vec![
            reply(r#"{"type":"workspace_list","workspaces":[]}"#),
            reply(
                r#"{"type":"workspace_created","workspace":{"workspace_id":"w9","number":9,"label":"sleeping","focused":false,"pane_count":1,"tab_count":1,"active_tab_id":"w9:t1","agent_status":"unknown"},"tab":{"tab_id":"w9:t1","workspace_id":"w9","number":1,"label":"1","focused":false,"pane_count":1,"agent_status":"unknown"},"root_pane":{"pane_id":"w9:p1","terminal_id":"term_w9p1","workspace_id":"w9","tab_id":"w9:t1","focused":false,"agent_status":"unknown","revision":1}}"#,
            ),
            reply(
                r#"{"type":"layout_apply","layout":{"workspace_id":"w9","tab_id":"w9:t2","zoomed":false,"focused_pane_id":"w9:p2","root":{"type":"pane","pane_id":"w9:p2","cwd":"/sleeping","command":["claude"]}}}"#,
            ),
            reply(r#"{"type":"tab_focus"}"#),
            reply(r#"{"type":"workspace_focus"}"#),
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
    let methods: Vec<_> = seen
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        methods,
        [
            "workspace.list",
            "workspace.create",
            "layout.apply",
            "tab.focus",
            "workspace.focus"
        ]
    );
    assert_eq!(seen[1]["params"]["label"], "sleeping");
    assert_eq!(seen[1]["params"]["focus"], false);
    // workspace.create が作った初期 tab を置換する。tab_id と workspace_id の
    // 併用は herdr が invalid_target で拒否するので tab_id 単独でなければならない
    assert_eq!(seen[2]["params"]["tab_id"], "w9:t1");
    assert!(seen[2]["params"].get("workspace_id").is_none());
    assert_eq!(seen[2]["params"]["focus"], false);
    // 旧形式は tab 名を持たないので workspace label を tab 名に使う (v0.1.4 互換)
    assert_eq!(seen[2]["params"]["tab_label"], "sleeping");
    assert_eq!(seen[2]["params"]["root"]["command"][0], "claude");
    // focus は全 tab が揃ってから: 置換後の tab_id → workspace の順
    assert_eq!(seen[3]["params"]["tab_id"], "w9:t2");
    assert_eq!(seen[4]["params"]["workspace_id"], "w9");
}

#[test]
fn picker_enter_restores_every_tab_of_a_multi_tab_definition() {
    let temp = TempDir::new("picker-tabs");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("agents.toml"),
        concat!(
            "label = \"agents\"\n\n",
            "[[tabs]]\nlabel = \"agent\"\n\n",
            "[tabs.root]\ntype = \"pane\"\ncwd = \"/work\"\ncommand = [\"claude\"]\n\n",
            "[[tabs]]\nlabel = \"docs\"\nactive = true\n\n",
            "[tabs.root]\ntype = \"pane\"\ncwd = \"/work/docs\"\ncommand = [\"codex\"]\n",
        ),
    )
    .unwrap();
    let fake_fzf = temp.0.join("fzf");
    fs::write(&fake_fzf, "#!/bin/sh\nprintf 'enter\\n○\\tagents\\n'\n").unwrap();
    fs::set_permissions(&fake_fzf, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = temp.0.join("herdr.sock");
    let (requests, server) = mock_herdr(
        &socket,
        vec![
            reply(r#"{"type":"workspace_list","workspaces":[]}"#),
            reply(
                r#"{"type":"workspace_created","workspace":{"workspace_id":"w9","number":9,"label":"agents","focused":false,"pane_count":1,"tab_count":1,"active_tab_id":"w9:t1","agent_status":"unknown"},"tab":{"tab_id":"w9:t1","workspace_id":"w9","number":1,"label":"1","focused":false,"pane_count":1,"agent_status":"unknown"},"root_pane":{"pane_id":"w9:p1","terminal_id":"term_w9p1","workspace_id":"w9","tab_id":"w9:t1","focused":false,"agent_status":"unknown","revision":1}}"#,
            ),
            reply(
                r#"{"type":"layout_apply","layout":{"workspace_id":"w9","tab_id":"w9:t2","zoomed":false,"focused_pane_id":"w9:p2","root":{"type":"pane","pane_id":"w9:p2","cwd":"/work","command":["claude"]}}}"#,
            ),
            reply(
                r#"{"type":"layout_apply","layout":{"workspace_id":"w9","tab_id":"w9:t3","zoomed":false,"focused_pane_id":"w9:p3","root":{"type":"pane","pane_id":"w9:p3","cwd":"/work/docs","command":["codex"]}}}"#,
            ),
            reply(r#"{"type":"tab_focus"}"#),
            reply(r#"{"type":"workspace_focus"}"#),
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
    let methods: Vec<_> = seen
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        methods,
        [
            "workspace.list",
            "workspace.create",
            "layout.apply",
            "layout.apply",
            "tab.focus",
            "workspace.focus"
        ]
    );
    // tab 1 は初期 tab の置換 (tab_id 単独)。兄弟がいる状態で置換すると末尾へ
    // 動くため、置換 → append の順でだけ保存順が保たれる (herdr 0.7.5 実測)
    assert_eq!(seen[2]["params"]["tab_id"], "w9:t1");
    assert!(seen[2]["params"].get("workspace_id").is_none());
    assert_eq!(seen[2]["params"]["tab_label"], "agent");
    assert_eq!(seen[2]["params"]["focus"], false);
    assert_eq!(seen[2]["params"]["root"]["command"][0], "claude");
    // tab 2 以降は workspace_id 単独の append
    assert_eq!(seen[3]["params"]["workspace_id"], "w9");
    assert!(seen[3]["params"].get("tab_id").is_none());
    assert_eq!(seen[3]["params"]["tab_label"], "docs");
    assert_eq!(seen[3]["params"]["focus"], false);
    assert_eq!(seen[3]["params"]["root"]["command"][0], "codex");
    // 全 tab が揃ってから、保存時に active だった tab (docs = 2 本目の実 tab_id)
    // を選び workspace を前面へ
    assert_eq!(seen[4]["params"]["tab_id"], "w9:t3");
    assert_eq!(seen[5]["params"]["workspace_id"], "w9");
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
