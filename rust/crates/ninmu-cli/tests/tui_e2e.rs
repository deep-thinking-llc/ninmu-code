use std::fs;
use std::io::{Read, Write};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

#[allow(dead_code)]
mod common;

static E2E_LOCK: Mutex<()> = Mutex::new(());

fn e2e_lock() -> std::sync::MutexGuard<'static, ()> {
    E2E_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_tui(
    label: &str,
) -> (
    Box<dyn MasterPty + Send>,
    Box<dyn Child + Send + Sync>,
    std::path::PathBuf,
) {
    let temp_dir = common::unique_temp_dir(label);
    let config_home = temp_dir.join("home").join(".ninmu");
    fs::create_dir_all(&config_home).expect("config home should exist");

    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty should open");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_ninmu"));
    cmd.arg("--model");
    cmd.arg("ollama/llama3.1:8b");
    cmd.arg("--tui");
    cmd.cwd(&temp_dir);
    cmd.env("NINMU_CONFIG_HOME", &config_home);
    cmd.env("NINMU_CONFIG_DIR", temp_dir.join(".ninmu"));
    cmd.env("NO_COLOR", "1");
    if label.contains("tool-error") {
        cmd.env("NINMU_TEST_SCRIPTED_TUI_TURN", "tool-error");
    } else if label.contains("permission") {
        cmd.env("NINMU_TEST_SCRIPTED_TUI_TURN", "permission");
    } else if label.contains("scripted") {
        cmd.env("NINMU_TEST_SCRIPTED_TUI_TURN", "1");
    }

    let child = pair.slave.spawn_command(cmd).expect("tui should spawn");
    drop(pair.slave);

    (pair.master, child, temp_dir)
}

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send(String::from_utf8_lossy(&buf[..n]).to_string())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn read_until(
    rx: &mpsc::Receiver<String>,
    needle: &str,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut output = String::new();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(chunk) => {
                output.push_str(&chunk);
                if strip_ansi(&output).contains(needle) {
                    return Ok(output);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Err(format!(
        "timed out waiting for {needle:?}; output:\n{output}"
    ))
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() || next == '~' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[test]
fn tui_starts_and_exits_cleanly() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-start-exit");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let output = read_until(&rx, "Ctrl+K", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let status = child.wait().expect("child should exit");

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    let output = output.expect("footer should render");
    assert!(strip_ansi(&output).contains("NINMU"));
    assert!(status.success(), "tui exit status: {status:?}");
}

#[test]
fn tui_help_overlay_surfaces_reasoning_and_model_controls() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-help");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(b"?");
    }
    let output = read_until(&rx, "/effort", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    let text = strip_ansi(&output.expect("help should include effort command"));
    assert!(text.contains("Ctrl+R"));
    assert!(text.contains("Ctrl+O"));
    assert!(text.contains("/model"));
    assert!(text.contains("/think"));
}

#[test]
fn tui_reasoning_overlay_opens_from_shortcut() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-reasoning");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(&[0x12]); // Ctrl+R
    }
    let output = read_until(&rx, "reasoning control", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    let text = strip_ansi(&output.expect("reasoning overlay should render"));
    assert!(text.contains("EFFORT"));
    assert!(text.contains("THINK"));
}

#[test]
fn tui_command_palette_lists_quality_of_life_actions() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-command-palette");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(&[0x0b]); // Ctrl+K
    }
    let output = read_until(&rx, "Stats", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    let text = strip_ansi(&output.expect("command palette should render"));
    assert!(text.contains("Sessions"));
    assert!(text.contains("Permissions"));
}

#[test]
fn tui_model_selector_surfaces_provider_and_context_metadata() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-model-selector");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(&[0x0f]); // Ctrl+O
    }
    let output = read_until(&rx, "provider", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    let text = strip_ansi(&output.expect("model selector should render"));
    assert!(text.contains("CTX"));
    assert!(text.contains("FAMILY"));
    assert!(text.contains("PRICE"));
    assert!(text.contains("CAP"));
    assert!(text.contains("provider"));
}

#[test]
fn tui_model_selector_enter_commits_current_model() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-model-selector-commit");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(&[0x0f]); // Ctrl+O
    }
    let selector = read_until(&rx, "provider", Duration::from_secs(5));
    if selector.is_ok() {
        let _ = writer.write_all(b"\r");
    }
    let output = read_until(&rx, "model set to ", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    selector.expect("model selector should render before commit");
    let text = strip_ansi(&output.expect("selected model should commit"));
    assert!(text.contains("model set to "), "captured text:\n{text}");
}

#[test]
fn tui_scripted_turn_shows_tool_progress_result_and_final_text() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-scripted-tool-turn");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(b"run scripted turn\r");
    }
    let output = read_until(&rx, "Scripted final response", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    let text = strip_ansi(&output.expect("scripted turn should complete"));
    assert!(text.contains("read_file"));
    assert!(text.contains("alpha line"));
}

#[test]
fn tui_scripted_permission_prompt_allows_and_returns_to_stream() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-permission-turn");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(b"needs permission\r");
    }
    let prompt = read_until(&rx, "permission required", Duration::from_secs(5));
    if prompt.is_ok() {
        let _ = writer.write_all(b"y");
    }
    let output = read_until(&rx, "Scripted permission allowed", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    let prompt_text = strip_ansi(&prompt.expect("permission prompt should render"));
    assert!(prompt_text.contains("bash"));
    assert!(prompt_text.contains("risk"));
    let text = strip_ansi(&output.expect("permission turn should complete"));
    assert!(text.contains("Scripted permission allowed"));
}

#[test]
fn tui_scripted_permission_prompt_view_then_deny_returns_to_stream() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-permission-view-deny");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(b"needs permission\r");
    }
    let prompt = read_until(&rx, "permission required", Duration::from_secs(5));
    if prompt.is_ok() {
        let _ = writer.write_all(b"v");
    }
    let viewed = read_until(&rx, r#""cmd":"cargo test""#, Duration::from_secs(5));
    if viewed.is_ok() {
        let _ = writer.write_all(b"d");
    }
    let output = read_until(&rx, "Scripted permission denied", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    prompt.expect("permission prompt should render");
    viewed.expect("view input should reveal command payload");
    let text = strip_ansi(&output.expect("permission denial should complete"));
    assert!(text.contains("denied by user"));
}

#[test]
fn tui_scripted_tool_result_expands_with_tab() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-scripted-expand");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(b"run scripted turn\r");
    }
    let finished = read_until(&rx, "Scripted final response", Duration::from_secs(5));
    if finished.is_ok() {
        let _ = writer.write_all(b"\t");
    }
    let output = read_until(&rx, "epsilon line", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    finished.expect("scripted turn should finish before expansion");
    let text = strip_ansi(&output.expect("expanded tool output should render"));
    assert!(text.contains("epsilon line"));
}

#[test]
fn tui_scripted_failing_tool_result_shows_failure_and_expands() {
    let _guard = e2e_lock();
    let (master, mut child, temp_dir) = spawn_tui("tui-tool-error");
    let rx = spawn_reader(master.try_clone_reader().expect("reader"));
    let mut writer = master.take_writer().expect("writer");

    let startup = read_until(&rx, "Ctrl+K", Duration::from_secs(5));
    if startup.is_ok() {
        let _ = writer.write_all(b"run failing scripted turn\r");
    }
    let finished = read_until(&rx, "Scripted failure handled", Duration::from_secs(5));
    if finished.is_ok() {
        let _ = writer.write_all(b"\t");
    }
    let output = read_until(&rx, "exit code 2", Duration::from_secs(5));

    let _ = writer.write_all(&[0x04]);
    let _ = child.kill();
    let _ = child.wait();

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");

    startup.expect("header/footer should render");
    let mut text =
        strip_ansi(&finished.expect("scripted failing turn should finish before expansion"));
    text.push_str(&strip_ansi(
        &output.expect("expanded failing tool output should render"),
    ));
    assert!(text.contains("fail bash"));
    assert!(text.contains("boom"));
    assert!(text.contains("exit code 2"));
}
