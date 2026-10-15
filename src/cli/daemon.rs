use super::client::Client;
use super::print;
use anyhow::anyhow;
use serde_json::json;
use std::process::{Command, Stdio};
use std::time::Duration;

pub(super) fn running_pid() -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(crate::home::pid_path()).ok()?.trim().parse().ok()?;
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success()
        .then_some(pid)
}

pub(super) async fn wait_healthy(url: &str) -> bool {
    let http = reqwest::Client::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if http
            .get(format!("{url}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return true;
        }
    }
    false
}

pub(super) async fn up(c: &Client, host: String, port: u16) -> anyhow::Result<()> {
    if let Some(pid) = running_pid() {
        print(&json!({ "status": "already running", "pid": pid, "url": c.url }));
        return Ok(());
    }
    let home = crate::home::home();
    std::fs::create_dir_all(&home)?;
    let log_path = home.join("nanolb.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["serve", "--host", &host, "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let child = cmd.spawn()?;
    let url = format!("http://{host}:{port}");
    if wait_healthy(&url).await {
        print(&json!({ "status": "up", "pid": child.id(), "url": url, "log": log_path }));
        return Ok(());
    }
    Err(anyhow!("nanolb did not become healthy within 5s; see {}", log_path.display()))
}

pub(super) async fn down() -> anyhow::Result<()> {
    let Some(pid) = running_pid() else {
        print(&json!({ "status": "not running" }));
        return Ok(());
    };
    Command::new("kill").args(["-INT", &pid.to_string()]).status()?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if running_pid().is_none() {
            print(&json!({ "status": "down", "pid": pid }));
            return Ok(());
        }
    }
    Err(anyhow!("nanolb (pid {pid}) did not stop within 10s"))
}
