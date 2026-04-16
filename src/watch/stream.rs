//! Supervisor for long-lived `stream = true` targets.
//!
//! Spawns children in their own process group so restart / shutdown can send
//! SIGTERM to the whole tree and avoid orphaned shells (e.g. `npm exec -- next dev`).

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::plugins::Target;

pub struct StreamChild {
    pub target_addr: String,
    child: Child,
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl StreamChild {
    /// Is the child still alive?
    pub fn poll(&mut self) -> Result<Option<i32>> {
        match self.child.try_wait()? {
            Some(status) => Ok(Some(status.code().unwrap_or(-1))),
            None => Ok(None),
        }
    }

    /// Send SIGTERM to the process group, then SIGKILL after the grace period.
    pub fn terminate(&mut self, grace: Duration) -> Result<()> {
        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid {
                unsafe {
                    libc::kill(-pgid, libc::SIGTERM);
                }
            } else {
                let _ = self.child.kill();
            }

            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if self.child.try_wait()?.is_some() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }

            if let Some(pgid) = self.pgid {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            } else {
                let _ = self.child.kill();
            }
        }

        #[cfg(not(unix))]
        {
            let _ = grace;
            let _ = self.child.kill();
        }

        let _ = self.child.wait();
        Ok(())
    }
}

pub struct StreamSupervisor {
    grace: Duration,
    children: HashMap<String, StreamChild>,
}

impl StreamSupervisor {
    pub fn new(grace: Duration) -> Self {
        Self {
            grace,
            children: HashMap::new(),
        }
    }

    pub fn is_running(&self, addr: &str) -> bool {
        self.children.contains_key(addr)
    }

    /// Spawn a stream target as a supervised long-lived process.
    pub fn spawn(&mut self, target_addr: &str, target: &Target, project_root: &Path) -> Result<()> {
        if self.children.contains_key(target_addr) {
            return Err(anyhow!("{target_addr} already running"));
        }

        let child = spawn_child(target, project_root)
            .with_context(|| format!("failed to spawn {target_addr}"))?;
        self.children.insert(target_addr.to_string(), child);
        Ok(())
    }

    /// Terminate a running target and respawn it.
    pub fn restart(
        &mut self,
        target_addr: &str,
        target: &Target,
        project_root: &Path,
    ) -> Result<()> {
        if let Some(mut existing) = self.children.remove(target_addr) {
            let _ = existing.terminate(self.grace);
        }
        self.spawn(target_addr, target, project_root)
    }

    pub fn shutdown_all(&mut self) {
        let addrs: Vec<String> = self.children.keys().cloned().collect();
        for addr in addrs {
            if let Some(mut child) = self.children.remove(&addr) {
                let _ = child.terminate(self.grace);
            }
        }
    }

    /// Poll each child and drop the entry if it exited.
    /// Returns the targets that exited since the last poll.
    pub fn reap(&mut self) -> Vec<(String, i32)> {
        let mut exited = Vec::new();
        let addrs: Vec<String> = self.children.keys().cloned().collect();
        for addr in addrs {
            if let Some(child) = self.children.get_mut(&addr) {
                if let Ok(Some(code)) = child.poll() {
                    self.children.remove(&addr);
                    exited.push((addr, code));
                }
            }
        }
        exited
    }
}

impl Drop for StreamSupervisor {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

fn spawn_child(target: &Target, project_root: &Path) -> Result<StreamChild> {
    let working_dir = target.working_dir.as_deref().unwrap_or(project_root);

    let parts: Vec<&str> = target.command.split_whitespace().collect();
    if parts.is_empty() {
        return Err(anyhow!("empty command"));
    }

    let mut env_vars: Vec<(&str, &str)> = Vec::new();
    let mut cmd_start = 0;
    for (i, part) in parts.iter().enumerate() {
        if let Some(eq_pos) = part.find('=') {
            let name = &part[..eq_pos];
            let is_env = !name.is_empty()
                && name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_alphabetic() || c == '_')
                    .unwrap_or(false)
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if is_env {
                env_vars.push((name, &part[eq_pos + 1..]));
                cmd_start = i + 1;
                continue;
            }
        }
        break;
    }

    if cmd_start >= parts.len() {
        return Err(anyhow!("empty command (only env vars)"));
    }

    let program = parts[cmd_start];
    let args = &parts[cmd_start + 1..];

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(working_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (name, value) in env_vars {
        cmd.env(name, value);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Create a new process group so we can SIGTERM the whole tree.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    // Fall back to setpgid if setsid fails (already a session leader).
                    let _ = libc::setpgid(0, 0);
                }
                Ok(())
            });
        }
    }

    let child = cmd.spawn().context("spawn failed")?;

    #[cfg(unix)]
    let pgid = Some(child.id() as i32);
    #[cfg(not(unix))]
    let pgid: Option<i32> = None;

    Ok(StreamChild {
        target_addr: String::new(),
        child,
        #[cfg(unix)]
        pgid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk_target(cmd: &str) -> Target {
        Target {
            command: cmd.to_string(),
            ..Default::default()
        }
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn spawn_and_poll_running_child() {
        let dir = tempdir();
        let mut sup = StreamSupervisor::new(Duration::from_secs(1));
        sup.spawn("//a:dev", &mk_target("sleep 5"), dir.path())
            .unwrap();
        assert!(sup.is_running("//a:dev"));

        // Child is alive — reap finds nothing.
        let exited = sup.reap();
        assert!(
            exited.is_empty(),
            "no children should have exited yet, got {exited:?}"
        );

        sup.shutdown_all();
        assert!(!sup.is_running("//a:dev"));
    }

    #[test]
    fn double_spawn_errors() {
        let dir = tempdir();
        let mut sup = StreamSupervisor::new(Duration::from_secs(1));
        sup.spawn("//a:dev", &mk_target("sleep 5"), dir.path())
            .unwrap();
        let err = sup
            .spawn("//a:dev", &mk_target("sleep 5"), dir.path())
            .unwrap_err();
        assert!(err.to_string().contains("already running"));
        sup.shutdown_all();
    }

    #[test]
    fn terminate_kills_within_grace_period() {
        let dir = tempdir();
        let mut sup = StreamSupervisor::new(Duration::from_millis(500));
        sup.spawn("//a:dev", &mk_target("sleep 30"), dir.path())
            .unwrap();

        let start = std::time::Instant::now();
        sup.shutdown_all();
        let elapsed = start.elapsed();

        // SIGTERM on `sleep` exits immediately — well under grace.
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown took too long: {elapsed:?}"
        );
        assert!(!sup.is_running("//a:dev"));
    }

    #[cfg(unix)]
    #[test]
    fn terminate_escalates_to_sigkill_for_trapping_child() {
        // Spawn a shell that traps SIGTERM and keeps running. Supervisor must
        // escalate to SIGKILL after the grace window. Script lives in a file
        // so aster's whitespace-split command parser won't mangle quoting.
        let dir = tempdir();
        let script_path = dir.path().join("trap.sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\ntrap '' TERM\nwhile true; do sleep 1; done\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }

        let mut sup = StreamSupervisor::new(Duration::from_millis(300));
        let target = mk_target(script_path.to_str().unwrap());
        sup.spawn("//a:dev", &target, dir.path()).unwrap();
        // Let the trap handler install before we SIGTERM.
        std::thread::sleep(Duration::from_millis(200));

        let start = std::time::Instant::now();
        sup.shutdown_all();
        let elapsed = start.elapsed();

        // SIGTERM ignored → grace elapses → SIGKILL. Expect shutdown to take
        // at least ~grace and complete within a small upper bound.
        assert!(
            elapsed >= Duration::from_millis(250),
            "shutdown returned before grace: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "shutdown exceeded reasonable bound: {elapsed:?}"
        );
        assert!(!sup.is_running("//a:dev"));
    }

    #[test]
    fn reap_returns_exit_codes_for_dead_children() {
        let dir = tempdir();
        let mut sup = StreamSupervisor::new(Duration::from_secs(1));
        // A child that exits immediately with code 0.
        sup.spawn("//a:dev", &mk_target("true"), dir.path()).unwrap();

        // Give the short-lived child time to exit.
        std::thread::sleep(Duration::from_millis(200));

        let exited = sup.reap();
        assert_eq!(exited.len(), 1);
        assert_eq!(exited[0].0, "//a:dev");
        assert_eq!(exited[0].1, 0);
        assert!(!sup.is_running("//a:dev"));
    }

    #[test]
    fn restart_stops_previous_and_spawns_new() {
        let dir = tempdir();
        let mut sup = StreamSupervisor::new(Duration::from_millis(500));
        sup.spawn("//a:dev", &mk_target("sleep 30"), dir.path())
            .unwrap();
        // First child is running.
        assert!(sup.is_running("//a:dev"));

        sup.restart("//a:dev", &mk_target("sleep 30"), dir.path())
            .unwrap();
        // Still running — but a *new* process.
        assert!(sup.is_running("//a:dev"));
        sup.shutdown_all();
    }

    #[test]
    fn drop_kills_outstanding_children() {
        let dir = tempdir();
        let start;
        {
            let mut sup = StreamSupervisor::new(Duration::from_millis(500));
            sup.spawn("//a:dev", &mk_target("sleep 30"), dir.path())
                .unwrap();
            start = std::time::Instant::now();
            // sup drops here
        }
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn empty_command_errors() {
        let dir = tempdir();
        let mut sup = StreamSupervisor::new(Duration::from_secs(1));
        let err = sup
            .spawn("//a:dev", &mk_target(""), dir.path())
            .unwrap_err();
        // anyhow wraps: outer context "failed to spawn //a:dev", inner "empty command".
        let full = format!("{err:#}");
        assert!(
            full.contains("empty command"),
            "expected empty-command error, got: {full}"
        );
    }

    #[test]
    fn spawn_respects_working_dir_override() {
        // `ls marker` succeeds iff cwd contains `marker`. No quoting needed.
        let outer = tempdir();
        let inner = outer.path().join("sub");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("marker"), "hi").unwrap();

        let target = Target {
            command: "ls marker".to_string(),
            working_dir: Some(inner.clone() as PathBuf),
            ..Default::default()
        };

        let mut sup = StreamSupervisor::new(Duration::from_millis(500));
        // Spawn pointing at `outer` as project_root — but working_dir override sends cwd to inner.
        sup.spawn("//a:dev", &target, outer.path()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let exited = sup.reap();
        assert_eq!(exited.len(), 1);
        assert_eq!(exited[0].1, 0, "child ran in wrong directory");
    }
}
