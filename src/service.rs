use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT: &str = include_str!("../contrib/nanolb.service");
const PLIST: &str = include_str!("../contrib/nanolb.plist");
const UNIT_PATH: &str = "/etc/systemd/system/nanolb.service";
const SYSTEM_BIN: &str = "/usr/local/bin/nanolb";
const SYSTEM_HOME: &str = "/var/lib/nanolb";
const LABEL: &str = "moe.pyraxo.nanolb";

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(cmd).args(args).status().map_err(|e| format!("{cmd}: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("`{cmd} {}` failed ({status})", args.join(" ")))
}

fn ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn render(template: &str, vars: &[(&str, &str)]) -> String {
    vars.iter().fold(template.to_string(), |t, (k, v)| t.replace(k, v))
}

fn replace_file(src: &Path, dst: &Path) -> Result<(), String> {
    if src == dst {
        return Ok(());
    }
    let tmp = dst.with_extension("new");
    std::fs::copy(src, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dst).map_err(|e| format!("install {}: {e}", dst.display()))
}

fn plist_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn uid() -> String {
    String::from_utf8(Command::new("id").arg("-u").output().map(|o| o.stdout).unwrap_or_default())
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn install(host: &str, port: u16) -> Result<Value, String> {
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).map_err(|e| e.to_string())?;
    let port = port.to_string();
    if cfg!(target_os = "macos") {
        let home = crate::home::home();
        std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        let log = home.join("nanolb.log");
        let plist = plist_path();
        std::fs::create_dir_all(plist.parent().unwrap()).map_err(|e| e.to_string())?;
        let vars = [
            ("@LABEL@", LABEL),
            ("@EXE@", exe.to_str().unwrap_or_default()),
            ("@HOST@", host),
            ("@PORT@", &port),
            ("@LOG@", log.to_str().unwrap_or_default()),
        ];
        let target = format!("gui/{}", uid());
        let _ = Command::new("launchctl").args(["bootout", &format!("{target}/{LABEL}")]).output();
        std::fs::write(&plist, render(PLIST, &vars)).map_err(|e| e.to_string())?;
        run("launchctl", &["bootstrap", &target, plist.to_str().unwrap_or_default()])?;
        return Ok(json!({
            "status": "installed", "manager": "launchd", "plist": plist, "binary": exe,
            "url": format!("http://{host}:{port}"), "log": log, "home": home,
        }));
    }
    if !ok("systemctl", &["--version"]) {
        return Err("no systemd or launchd found; run `nanolb serve` under your own supervisor".into());
    }
    if uid() != "0" {
        return Err("installing the systemd service needs root: sudo nanolb service install".into());
    }
    replace_file(&exe, Path::new(SYSTEM_BIN))?;
    if !ok("id", &["nanolb"]) {
        run(
            "useradd",
            &[
                "--system",
                "--no-create-home",
                "--home-dir",
                SYSTEM_HOME,
                "--shell",
                "/usr/sbin/nologin",
                "nanolb",
            ],
        )?;
    }
    let vars = [("@EXE@", SYSTEM_BIN), ("@HOST@", host), ("@PORT@", &port)];
    std::fs::write(UNIT_PATH, render(UNIT, &vars)).map_err(|e| format!("write {UNIT_PATH}: {e}"))?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "--now", "nanolb"])?;
    Ok(json!({
        "status": "installed", "manager": "systemd", "unit": UNIT_PATH, "binary": SYSTEM_BIN,
        "url": format!("http://{host}:{port}"), "home": SYSTEM_HOME, "logs": "journalctl -u nanolb -f",
        "cli": format!("sudo -u nanolb NANOLB_HOME={SYSTEM_HOME} nanolb status"),
    }))
}

pub fn uninstall(purge: bool) -> Result<Value, String> {
    if cfg!(target_os = "macos") {
        let plist = plist_path();
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
            .output();
        let removed = std::fs::remove_file(&plist).is_ok();
        if purge {
            let _ = std::fs::remove_dir_all(crate::home::home());
        }
        return Ok(json!({"status": "uninstalled", "manager": "launchd", "plist_removed": removed, "data_purged": purge}));
    }
    if uid() != "0" {
        return Err("removing the systemd service needs root: sudo nanolb service uninstall".into());
    }
    let _ = Command::new("systemctl").args(["disable", "--now", "nanolb"]).status();
    let removed = std::fs::remove_file(UNIT_PATH).is_ok();
    run("systemctl", &["daemon-reload"])?;
    if purge {
        let _ = std::fs::remove_dir_all(SYSTEM_HOME);
        let _ = Command::new("userdel").arg("nanolb").status();
        let _ = std::fs::remove_file(SYSTEM_BIN);
    }
    Ok(json!({"status": "uninstalled", "manager": "systemd", "unit_removed": removed, "data_purged": purge}))
}
