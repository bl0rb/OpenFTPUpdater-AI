//! Running external tools (git, npm, claude) with the user's PATH, line by line and cancellable.
//!
//! An app started from the Finder only gets `/usr/bin:/bin:…`; tools from nvm, Homebrew or
//! `~/.local/bin` would be missing. `user_path` asks the login shell once for the real PATH.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// PATH of the user's interactive login shell, with common tool folders as fallback.
pub fn user_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let mut parts: Vec<String> = Vec::new();
        if let Some(path) = login_shell_path() {
            parts.extend(path.split(':').map(str::to_owned));
        }
        if let Ok(current) = std::env::var("PATH") {
            parts.extend(current.split(':').map(str::to_owned));
        }
        if let Some(home) = dirs::home_dir() {
            parts.push(home.join(".local/bin").display().to_string());
        }
        parts.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(str::to_owned));
        let mut seen = std::collections::HashSet::new();
        parts.retain(|p| !p.is_empty() && seen.insert(p.clone()));
        parts.join(":")
    })
}

fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", "printf '__OFTPU_PATH__%s__END__' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(10) => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let start = out.find("__OFTPU_PATH__")? + "__OFTPU_PATH__".len();
    let end = out[start..].find("__END__")? + start;
    Some(out[start..end].to_owned())
}

fn command(program: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd).env("PATH", user_path()).stdin(Stdio::null());
    cmd
}

/// Runs a command to completion and returns stdout; fails with stderr on a non-zero exit.
pub fn output(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let out = command(program, cwd).args(args).output().with_context(|| format!("{program} nicht gefunden"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let message = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        bail!("{program} {}: {}", args.join(" "), message);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

/// The process of the step that is currently running; `cancel` stops it and everything it started.
#[derive(Default)]
pub struct Runner {
    cancelled: AtomicBool,
    pid: Mutex<Option<u32>>,
}

impl Runner {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pid.lock().unwrap_or_else(|e| e.into_inner()) {
            kill_group(pid);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("Abgebrochen");
        }
        Ok(())
    }
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    // The child leads its own process group (see `stream`), so npm/claude subprocesses stop too.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn kill_group(pid: u32) {
    let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output();
}

/// Runs a command and hands every stdout/stderr line to `on_line` as it arrives. Returns the exit code.
pub fn stream(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    runner: &Runner,
    mut on_line: impl FnMut(&str),
) -> Result<i32> {
    runner.check()?;
    let mut cmd = command(program, cwd);
    cmd.args(args).envs(env).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().with_context(|| format!("{program} konnte nicht gestartet werden"))?;
    *runner.pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(child.id());

    let (tx, rx) = mpsc::channel::<String>();
    let readers: Vec<_> = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
        .into_iter()
        .flatten()
        .map(|pipe| {
            let tx = tx.clone();
            thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();
    drop(tx);
    for line in rx {
        on_line(&line);
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child.wait()?;
    *runner.pid.lock().unwrap_or_else(|e| e.into_inner()) = None;
    runner.check()?;
    Ok(status.code().unwrap_or(-1))
}

/// A shell command line from site.json (e.g. `npm run build`), run through `/bin/sh -c`.
pub fn stream_shell(
    command_line: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    runner: &Runner,
    on_line: impl FnMut(&str),
) -> Result<i32> {
    #[cfg(unix)]
    return stream("/bin/sh", &["-c".into(), command_line.into()], cwd, env, runner, on_line);
    #[cfg(not(unix))]
    return stream("cmd", &["/C".into(), command_line.into()], cwd, env, runner, on_line);
}
