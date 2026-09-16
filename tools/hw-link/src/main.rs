//! Keeps the PC reachable from HQ.
//!
//! The PC dials out to HQ and holds a reverse forward, so HQ can `ssh -p 2222 <user>@127.0.0.1`
//! and land on this machine. No inbound ports, no WireGuard, no VPN.
//!
//! This replaces the PowerShell version, which needed an elevated shell, a SYSTEM scheduled task
//! and ACL surgery on the key, and which failed silently every time one of those went wrong.
//! Here the tunnel runs as the logged-in user, in a window they can see and close.
//!
//!   hw-link.exe              hold the tunnel until the window closes
//!   hw-link.exe --install    also start it at every login
//!   hw-link.exe --uninstall  stop starting it at login
//!
//! Windows ships OpenSSH, so ssh.exe does the protocol work; this only supervises it.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HQ_HOST: &str = "178.104.2.34";
const HQ_USER: &str = "hwtunnel";
const PORT: u16 = 2222;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "HardwaveLink";

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn key_path() -> PathBuf {
    home().join(".ssh").join("hwlink")
}

fn stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

fn say(line: &str) {
    println!("[{}] {}", stamp(), line);
}

/// Make the key if there is none. A brand new key is not authorised on HQ yet, so print it and
/// say so plainly: that is the one manual step, and it happens once per machine.
fn ensure_key() -> Result<bool, String> {
    let key = key_path();
    if key.exists() {
        return Ok(false);
    }
    if let Some(dir) = key.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    say("No tunnel key yet, making one.");
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "hardwave-link", "-f"])
        .arg(&key)
        .output()
        .map_err(|e| format!("ssh-keygen could not run: {e}"))?;
    if !out.status.success() {
        return Err(format!("ssh-keygen failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(true)
}

fn print_public_key() {
    let pub_path = key_path().with_extension("pub");
    match std::fs::read_to_string(&pub_path) {
        Ok(pubkey) => {
            println!();
            println!("This machine is not authorised on HQ yet. Send this line over, once:");
            println!();
            println!("  {}", pubkey.trim());
            println!();
        }
        Err(e) => say(&format!("could not read {}: {e}", pub_path.display())),
    }
}

fn install(remove: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("where am I: {e}"))?;
    let args: Vec<String> = if remove {
        vec!["delete".into(), format!(r"HKCU\{RUN_KEY}"), "/v".into(), RUN_NAME.into(), "/f".into()]
    } else {
        vec![
            "add".into(), format!(r"HKCU\{RUN_KEY}"), "/v".into(), RUN_NAME.into(),
            "/t".into(), "REG_SZ".into(), "/d".into(), format!("\"{}\"", exe.display()), "/f".into(),
        ]
    };
    let out = Command::new("reg").args(&args).output().map_err(|e| format!("reg: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    say(if remove { "Removed from login." } else { "Starts at login from now on." });
    Ok(())
}

/// One ssh attempt. Returns when the connection ends, whatever the reason.
fn run_tunnel() -> String {
    let forward = format!("127.0.0.1:{PORT}:127.0.0.1:22");
    let target = format!("{HQ_USER}@{HQ_HOST}");
    let status = Command::new("ssh")
        .args(["-N", "-R", &forward, "-i"])
        .arg(key_path())
        .args([
            "-o", "IdentitiesOnly=yes",
            "-o", "StrictHostKeyChecking=no",
            // SYSTEM and fresh accounts have no known_hosts, and the prompt would hang forever.
            "-o", "UserKnownHostsFile=NUL",
            "-o", "ExitOnForwardFailure=yes",
            // Drop a dead link quickly so the retry loop can take over.
            "-o", "ServerAliveInterval=30",
            "-o", "ServerAliveCountMax=3",
            "-o", "ConnectTimeout=15",
            &target,
        ])
        .status();
    match status {
        Ok(s) if s.success() => "the tunnel closed".to_string(),
        Ok(s) => format!("ssh stopped with code {}", s.code().unwrap_or(-1)),
        Err(e) => format!("could not start ssh: {e}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--uninstall") {
        if let Err(e) = install(true) {
            say(&format!("could not remove it from login: {e}"));
        }
        return;
    }
    if args.iter().any(|a| a == "--install") {
        if let Err(e) = install(false) {
            say(&format!("could not add it to login: {e}"));
        }
    }

    println!("Hardwave link");
    println!("Holding a way in to this PC for support. Close this window to stop it.");
    println!();

    match ensure_key() {
        Ok(true) => print_public_key(),
        Ok(false) => {}
        Err(e) => {
            say(&format!("no key: {e}"));
            return;
        }
    }

    say(&format!("connecting to {HQ_HOST}"));
    let mut announced = false;
    loop {
        let started = SystemTime::now();
        let why = run_tunnel();
        let lasted = started.elapsed().unwrap_or_default();

        // A connection that held for a while was a real one; a burst of instant failures means
        // the key is not authorised yet, or HQ is unreachable. Say which, rather than looping mute.
        if lasted > Duration::from_secs(20) {
            say(&format!("{why} after {} s, reconnecting", lasted.as_secs()));
            announced = false;
        } else {
            if !announced {
                say(&format!("{why}. If this repeats, the key above is not authorised on HQ yet."));
                announced = true;
            }
        }
        std::thread::sleep(Duration::from_secs(10));
    }
}
