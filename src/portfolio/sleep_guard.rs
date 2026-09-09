//! Keep the host awake for as long as this process runs.
//!
//! The momentum trader's trailing stop only exists while the monitor loop ticks, so a
//! suspended host is a blind stop — not a slow one. On 2026-09-09 this machine spent
//! **16.3 h of 22.5 h asleep** (94 macOS maintenance-sleep episodes, ~16 min in every 17),
//! during which no stop could fire and a manually-bought position took 23 minutes to be
//! adopted. `tick_timing::dark_secs` makes that *visible*; this module makes it *stop*.
//!
//! ## Why a watcher child and not a wrapper
//!
//! The obvious shape is to re-exec ourselves under `caffeinate`. We don't, for two reasons:
//! the arb binary already re-execs itself on SIGHUP (same PID, no supervisor) and stacking a
//! second exec wrapper on that invites subtle breakage; and a wrapper cannot be declined at
//! runtime. Instead we spawn a child that holds the inhibitor and watches OUR pid, which
//! also means the inhibitor is released even if we are `SIGKILL`ed — `kill_on_drop` does not
//! run on a hard kill, so a design that relied on it would leak the assertion (this repo has
//! already been bitten by exactly that with orphaned scan children).
//!
//! Fails open everywhere: a missing tool, an unsupported OS or a spawn error logs and
//! continues. A headless Linux server usually has no idle-sleep timer at all, so the common
//! server case is a no-op by design rather than an error.

use tracing::{info, warn};

/// Holds the inhibitor for its lifetime. Keep it alive in `main` — dropping it releases the
/// assertion. `kill_on_drop` handles the graceful path; the child's own pid-watch handles
/// the ungraceful one.
pub struct SleepGuard {
    _child: Option<tokio::process::Child>,
}

/// The command that holds a sleep inhibitor tied to `pid` on `os`, or `None` where we have
/// no mechanism. Pure so both platforms can be asserted from either host.
///
/// * **macOS** — `caffeinate -i -s -w <pid>`. `-i` blocks idle sleep, `-s` blocks system
///   sleep (AC only, per caffeinate(8)), and `-w` releases the assertion when `pid` exits.
///   Note this does NOT prevent *clamshell* sleep: closing the lid still suspends, and only
///   `sudo pmset -c disablesleep 1` stops that.
/// * **Linux** — `systemd-inhibit --mode=block`, which has no `-w` equivalent, so the
///   inhibited "command" is a POSIX poll on our pid that reproduces those semantics.
///
/// Injection-safe by construction: `reason` is passed as its own argv element (never through
/// a shell) and the only value interpolated into the `sh -c` script is a `u32` pid.
pub fn inhibitor_cmd(os: &str, pid: u32, reason: &str) -> Option<(String, Vec<String>)> {
    match os {
        "macos" => Some((
            "caffeinate".to_string(),
            vec!["-i".into(), "-s".into(), "-w".into(), pid.to_string()],
        )),
        "linux" => Some((
            "systemd-inhibit".to_string(),
            vec![
                "--what=sleep:idle:handle-lid-switch".into(),
                "--who=portfolio-watcher".into(),
                format!("--why={reason}"),
                "--mode=block".into(),
                "sh".into(),
                "-c".into(),
                format!("while kill -0 {pid} 2>/dev/null; do sleep 5; done"),
            ],
        )),
        _ => None,
    }
}

/// Spawn the host-sleep inhibitor. `enabled == false` (env `INHIBIT_HOST_SLEEP=false`)
/// returns an inert guard. Never fails: every error path logs and yields an inert guard,
/// because being unable to inhibit sleep must not stop the trader from running.
pub fn inhibit(enabled: bool, reason: &str) -> SleepGuard {
    if !enabled {
        info!("sleep guard: disabled (INHIBIT_HOST_SLEEP=false) — the host may sleep and the trailing stop with it");
        return SleepGuard { _child: None };
    }
    let pid = std::process::id();
    let Some((cmd, args)) = inhibitor_cmd(std::env::consts::OS, pid, reason) else {
        info!(
            "sleep guard: no inhibitor for this platform ({}) — relying on the host not to sleep",
            std::env::consts::OS
        );
        return SleepGuard { _child: None };
    };
    match tokio::process::Command::new(&cmd)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => {
            info!("sleep guard: host sleep inhibited via `{cmd}` for pid {pid} — released when this process exits");
            if std::env::consts::OS == "macos" {
                info!("sleep guard: note — this does NOT stop clamshell (lid-close) sleep; for that use `sudo pmset -c disablesleep 1`");
            }
            SleepGuard { _child: Some(child) }
        }
        Err(e) => {
            warn!(
                "sleep guard: could not start `{cmd}` ({e}) — the host may sleep and the trailing stop with it. \
                 On Linux this usually just means systemd-inhibit is absent (a headless server has no idle-sleep timer anyway)."
            );
            SleepGuard { _child: None }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_ties_caffeinate_to_our_pid() {
        let (cmd, args) = inhibitor_cmd("macos", 4242, "trailing stop").expect("macOS supported");
        assert_eq!(cmd, "caffeinate");
        // -w is what makes the assertion die with us even on SIGKILL.
        assert_eq!(args, vec!["-i", "-s", "-w", "4242"]);
    }

    #[test]
    fn linux_blocks_sleep_and_self_terminates_on_our_pid() {
        let (cmd, args) = inhibitor_cmd("linux", 4242, "trailing stop").expect("linux supported");
        assert_eq!(cmd, "systemd-inhibit");
        assert!(args.contains(&"--mode=block".to_string()), "advisory mode would not block sleep");
        assert!(args.iter().any(|a| a.starts_with("--what=") && a.contains("sleep")));
        assert!(args.iter().any(|a| a == "--why=trailing stop"), "reason is its own argv element");
        // systemd-inhibit has no `caffeinate -w`, so the inhibited command must watch our pid
        // itself — otherwise the inhibitor outlives us and holds the machine awake forever.
        assert!(args.last().expect("script").contains("kill -0 4242"));
    }

    #[test]
    fn unsupported_platform_yields_no_inhibitor() {
        assert!(inhibitor_cmd("windows", 1, "x").is_none());
        assert!(inhibitor_cmd("freebsd", 1, "x").is_none());
    }

    /// Live check that the OS actually grants the assertion — the one thing the pure tests
    /// above cannot cover. `#[ignore]`d because it depends on platform tooling (`pmset`);
    /// run it by hand after touching `inhibitor_cmd`:
    ///   `cargo test --lib sleep_guard -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_macos_assertion_is_actually_held() {
        if std::env::consts::OS != "macos" {
            eprintln!("skipped: macOS-only");
            return;
        }
        let guard = inhibit(true, "sleep_guard self-test");
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let out = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset runs");
        let text = String::from_utf8_lossy(&out.stdout);
        // Assert on the OWNING-PROCESS list, not the system-wide counters: other apps hold
        // idle assertions too (iBooks did during development), so a non-zero counter proves
        // nothing about us. `-s`/PreventSystemSleep additionally only registers on AC power,
        // which would make an assertion on it fail on battery for no real reason.
        assert!(
            text.contains("caffeinate"),
            "no caffeinate assertion is held — the guard did not take effect:\n{text}"
        );
        drop(guard);
        // And it must be RELEASED on drop, or the guard would wedge the machine awake.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let after = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset runs");
        assert!(
            !String::from_utf8_lossy(&after.stdout).contains("caffeinate"),
            "the caffeinate assertion outlived the guard"
        );
    }

    #[test]
    fn reason_cannot_break_out_of_the_linux_shell_script() {
        // The script interpolates only the pid; the reason rides in its own argv element,
        // so even a hostile reason cannot reach `sh -c`.
        let (_, args) = inhibitor_cmd("linux", 7, "x\"; rm -rf /; #").expect("linux supported");
        assert!(!args.last().unwrap().contains("rm -rf"));
        assert_eq!(args.last().unwrap(), "while kill -0 7 2>/dev/null; do sleep 5; done");
    }
}
