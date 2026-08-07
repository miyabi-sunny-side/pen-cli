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
            // macOS (BSD) では accept した stream が listener の non-blocking を
            // 継承する。client の書き込み前に read すると EAGAIN で落ちるため、
            // blocking へ戻して要求の到着を待つ (Linux は元々 blocking)
            stream.set_nonblocking(false).unwrap();
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
    // 保存定義と実 layout が一致するケース: 比較のための snapshot は走るが、
    // 質問なしで閉じる (legacy 形式は tab label = workspace label に正規化される)
    let original = "label = \"demo\"\n\n[root]\ntype = \"pane\"\ncwd = \"/work\"\n";
    fs::write(config.join("demo.toml"), original).unwrap();
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
            reply(
                r#"{"type":"tab_list","tabs":[{"tab_id":"w2:t1","workspace_id":"w2","number":1,"label":"demo","focused":true,"pane_count":1,"agent_status":"unknown"}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w2","tab_id":"w2:t1","zoomed":false,"focused_pane_id":"w2:p1","root":{"type":"pane","pane_id":"w2:p1","cwd":"/work"}}}"#,
            ),
            reply(
                r#"{"type":"pane_process_info","process_info":{"pane_id":"w2:p1","shell_pid":100,"foreground_process_group_id":100,"foreground_processes":[{"pid":100,"name":"zsh","argv":["/usr/bin/zsh"],"cmdline":"/usr/bin/zsh","cwd":"/work"}]}}"#,
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
    assert_eq!(
        fs::read_to_string(config.join("demo.toml")).unwrap(),
        original
    );
    let seen = requests.lock().unwrap();
    assert_eq!(seen[5]["method"], "workspace.close");
    assert_eq!(seen[5]["params"]["workspace_id"], "w2");
}

/// 三択 prompt へ1行流し込んで pen close を実行する
fn close_with_answer(temp: &TempDir, socket: &Path, answer: &str) -> std::process::Output {
    let mut child = pen(temp, socket)
        .arg("close")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answer.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn close_discard_closes_an_unsaved_workspace_without_saving() {
    let temp = TempDir::new("close-discard");
    let socket = temp.0.join("herdr.sock");
    let (requests, server) = mock_herdr(
        &socket,
        vec![
            reply(
                r#"{"type":"pane_current","pane":{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1"}}"#,
            ),
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w2","active_tab_id":"w2:t1","label":"scratch","focused":true}]}"#,
            ),
            reply(r#"{"type":"workspace_closed","workspace_id":"w2"}"#),
        ],
    );

    let output = close_with_answer(&temp, &socket, "d\n");
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[s]ave") && stderr.contains("[d]iscard") && stderr.contains("[c]ancel"),
        "stderr should offer the three close choices: {stderr}"
    );
    // discard は snapshot 系 RPC を一切呼ばず、TOML も作らない
    let methods: Vec<_> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        methods,
        ["pane.current", "workspace.list", "workspace.close"]
    );
    assert!(!temp.0.join("config").exists());
}

#[test]
fn close_asks_when_the_workspace_differs_from_its_saved_definition() {
    let temp = TempDir::new("close-differs");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    // 保存定義は /old、実 layout は /work: 黙って閉じずに質問し、
    // discard なら保存定義を書き換えないまま閉じる
    let original = "label = \"demo\"\n\n[root]\ntype = \"pane\"\ncwd = \"/old\"\n";
    fs::write(config.join("demo.toml"), original).unwrap();
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
            reply(
                r#"{"type":"tab_list","tabs":[{"tab_id":"w2:t1","workspace_id":"w2","number":1,"label":"demo","focused":true,"pane_count":1,"agent_status":"unknown"}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w2","tab_id":"w2:t1","zoomed":false,"focused_pane_id":"w2:p1","root":{"type":"pane","pane_id":"w2:p1","cwd":"/work"}}}"#,
            ),
            reply(
                r#"{"type":"pane_process_info","process_info":{"pane_id":"w2:p1","shell_pid":100,"foreground_process_group_id":100,"foreground_processes":[{"pid":100,"name":"zsh","argv":["/usr/bin/zsh"],"cmdline":"/usr/bin/zsh","cwd":"/work"}]}}"#,
            ),
            reply(r#"{"type":"workspace_closed","workspace_id":"w2"}"#),
        ],
    );

    let output = close_with_answer(&temp, &socket, "d\n");
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("differs") && stderr.contains("[u]pdate"),
        "stderr should ask about the mismatch: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(config.join("demo.toml")).unwrap(),
        original
    );
    let seen = requests.lock().unwrap();
    assert_eq!(seen.last().unwrap()["method"], "workspace.close");
}

#[test]
fn close_update_rewrites_the_saved_definition_before_closing() {
    let temp = TempDir::new("close-update");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("demo.toml"),
        "label = \"demo\"\n\n[root]\ntype = \"pane\"\ncwd = \"/old\"\n",
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
            reply(
                r#"{"type":"tab_list","tabs":[{"tab_id":"w2:t1","workspace_id":"w2","number":1,"label":"demo","focused":true,"pane_count":1,"agent_status":"unknown"}]}"#,
            ),
            reply(
                r#"{"type":"layout_export","layout":{"workspace_id":"w2","tab_id":"w2:t1","zoomed":false,"focused_pane_id":"w2:p1","root":{"type":"pane","pane_id":"w2:p1","cwd":"/work"}}}"#,
            ),
            reply(
                r#"{"type":"pane_process_info","process_info":{"pane_id":"w2:p1","shell_pid":100,"foreground_process_group_id":100,"foreground_processes":[{"pid":100,"name":"zsh","argv":["/usr/bin/zsh"],"cmdline":"/usr/bin/zsh","cwd":"/work"}]}}"#,
            ),
            reply(r#"{"type":"workspace_closed","workspace_id":"w2"}"#),
        ],
    );

    let output = close_with_answer(&temp, &socket, "u\n");
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let updated = fs::read_to_string(config.join("demo.toml")).unwrap();
    assert!(
        updated.contains("cwd = \"/work\"") && !updated.contains("/old"),
        "definition should be rewritten from the live layout: {updated}"
    );
    let seen = requests.lock().unwrap();
    assert_eq!(seen.last().unwrap()["method"], "workspace.close");
}

#[test]
fn picker_space_restores_a_legacy_definition_and_keeps_the_picker_open() {
    let temp = TempDir::new("picker");
    let config = temp.0.join("config");
    fs::create_dir_all(&config).unwrap();
    // v0.1.x の旧形式 (単一 [root]) も 1 tab の workspace として復元できること
    fs::write(
        config.join("sleeping.toml"),
        "label = \"sleeping\"\n\n[root]\ntype = \"pane\"\ncwd = \"/sleeping\"\ncommand = [\"claude\"]\n",
    )
    .unwrap();
    // Space はトグル後も picker が続く: fzf は2回起動される (1回目 Space →
    // 2回目 Esc)。fake fzf は状態ファイルで自分の起動回数を数える
    let fake_fzf = temp.0.join("fzf");
    fs::write(
        &fake_fzf,
        "#!/bin/sh\nif [ -f \"$0.ran\" ]; then\n  printf 'esc\\n'\nelse\n  : > \"$0.ran\"\n  printf 'space\\n○\\tsleeping\\n'\nfi\n",
    )
    .unwrap();
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
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w9","active_tab_id":"w9:t2","label":"sleeping","focused":false}]}"#,
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
    let methods: Vec<_> = seen
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect();
    // Space の復元は workspace.focus を呼ばず (連続操作の妨げになる)、
    // トグル後に一覧を作り直すため workspace.list をもう一度取る
    assert_eq!(
        methods,
        [
            "workspace.list",
            "workspace.create",
            "layout.apply",
            "tab.focus",
            "workspace.list"
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
    // active tab の選択は workspace 内で閉じ、focus は移さない
    assert_eq!(seen[3]["params"]["tab_id"], "w9:t2");
}

#[test]
fn picker_space_then_enter_closes_two_running_workspaces() {
    let temp = TempDir::new("picker-running");
    let socket = temp.0.join("herdr.sock");
    // user の主目的「2個消して次を操作」の稼働中(●)側: 1回目 Space の close 後も
    // picker が続き、2回目 Enter は close して終了する。Enter+稼働中が旧挙動の
    // workspace.focus に戻る退行もここで検知する
    let fake_fzf = temp.0.join("fzf");
    fs::write(
        &fake_fzf,
        "#!/bin/sh\nif [ -f \"$0.ran\" ]; then\n  printf 'enter\\n●\\tbeta\\n'\nelse\n  : > \"$0.ran\"\n  printf 'space\\n●\\talpha\\n'\nfi\n",
    )
    .unwrap();
    fs::set_permissions(&fake_fzf, fs::Permissions::from_mode(0o755)).unwrap();
    let (requests, server) = mock_herdr(
        &socket,
        vec![
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w1","active_tab_id":"w1:t1","label":"alpha","focused":true},{"workspace_id":"w2","active_tab_id":"w2:t1","label":"beta","focused":false}]}"#,
            ),
            reply(r#"{"type":"workspace_closed","workspace_id":"w1"}"#),
            reply(
                r#"{"type":"workspace_list","workspaces":[{"workspace_id":"w2","active_tab_id":"w2:t1","label":"beta","focused":true}]}"#,
            ),
            reply(r#"{"type":"workspace_closed","workspace_id":"w2"}"#),
        ],
    );

    let mut child = pen(&temp, &socket)
        .env("PEN_FZF", &fake_fzf)
        .arg("picker")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // どちらも未保存なので三択が2回出る: 両方 discard で閉じる
    child.stdin.take().unwrap().write_all(b"d\nd\n").unwrap();
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("closed alpha") && stdout.contains("closed beta"),
        "both workspaces should be closed: {stdout}"
    );
    let seen = requests.lock().unwrap();
    let methods: Vec<_> = seen
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect();
    // Space 後に一覧を作り直すため workspace.list を再取得し、Enter 後は終了。
    // workspace.focus はどこにも現れない
    assert_eq!(
        methods,
        [
            "workspace.list",
            "workspace.close",
            "workspace.list",
            "workspace.close"
        ]
    );
    assert_eq!(seen[1]["params"]["workspace_id"], "w1");
    assert_eq!(seen[3]["params"]["workspace_id"], "w2");
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
