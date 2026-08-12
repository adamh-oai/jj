//! Fenced execution of jj's one supported background snapshot trigger.

use crate::facade::FacadeService;
use crate::manager::TriggerRun;
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct TriggerCommandConfig {
    pub jj_executable: PathBuf,
    pub daemon_socket: PathBuf,
    pub home: PathBuf,
    pub requester_uid: u32,
    pub requester_gid: u32,
    pub run_owner: [u8; 16],
    pub lease_ns: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerOutcome {
    pub watch_id: [u8; 16],
    pub through_sequence: i64,
    pub succeeded: bool,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct ClaimedTrigger {
    run: TriggerRun,
    root: PathBuf,
}

pub fn claim_one_pending(
    facade: &mut FacadeService,
    config: &TriggerCommandConfig,
    now_ns: i64,
) -> Result<Option<ClaimedTrigger>, TriggerError> {
    validate_process_identity(config)?;
    let expires = now_ns
        .checked_add(config.lease_ns)
        .ok_or_else(|| TriggerError::new("trigger lease expiration overflow"))?;
    let run = facade
        .service_mut()
        .store_mut()
        .claim_fixed_jj_trigger(config.run_owner, now_ns, expires)
        .map_err(|error| TriggerError::context("claim jj trigger", error))?;
    let Some(run) = run else {
        return Ok(None);
    };
    let resolved = (|| {
        let durable_root = facade
            .service()
            .store()
            .fixed_jj_trigger_root(&run, config.requester_uid)
            .map_err(|error| TriggerError::context("authorize jj trigger root", error))?;
        let monitored_root = facade
            .verified_view_root(run.watch_id)
            .map_err(|error| TriggerError::context("verify jj trigger view", error))?;
        if monitored_root.as_os_str().as_bytes() != durable_root {
            return Err(TriggerError::new(
                "trigger's durable root differs from its monitored root",
            ));
        }
        Ok(monitored_root)
    })();
    let monitored_root = match resolved {
        Ok(root) => root,
        Err(error) => {
            let release = facade
                .service_mut()
                .store_mut()
                .finish_fixed_jj_trigger(&run, false);
            return Err(match release {
                Ok(()) => error,
                Err(release) => TriggerError::new(format!(
                    "{error}; release rejected jj trigger claim: {release}"
                )),
            });
        }
    };
    Ok(Some(ClaimedTrigger {
        run,
        root: monitored_root,
    }))
}

/// Executes the fixed command without holding the facade lock. The child may
/// therefore connect back to the same daemon for its ordinary Watchman query.
pub fn execute_claimed(
    config: &TriggerCommandConfig,
    claimed: &ClaimedTrigger,
) -> Result<std::process::ExitStatus, TriggerError> {
    execute_jj(config, &claimed.root)
}

pub fn finish_claimed(
    facade: &mut FacadeService,
    claimed: ClaimedTrigger,
    execution: Result<std::process::ExitStatus, TriggerError>,
) -> Result<TriggerOutcome, TriggerError> {
    let succeeded = execution
        .as_ref()
        .is_ok_and(std::process::ExitStatus::success);
    let finish = facade
        .service_mut()
        .store_mut()
        .finish_fixed_jj_trigger(&claimed.run, succeeded);
    if let Err(error) = execution {
        return Err(match finish {
            Ok(()) => error,
            Err(finish) => {
                TriggerError::new(format!("{error}; release jj trigger claim: {finish}"))
            }
        });
    }
    finish.map_err(|error| TriggerError::context("finish jj trigger", error))?;
    let status = execution.expect("execution error returned above");
    Ok(TriggerOutcome {
        watch_id: claimed.run.watch_id,
        through_sequence: claimed.run.through_sequence,
        succeeded,
        exit_code: status.code(),
    })
}

pub fn run_one_pending(
    facade: &mut FacadeService,
    config: &TriggerCommandConfig,
    now_ns: i64,
) -> Result<Option<TriggerOutcome>, TriggerError> {
    let Some(claimed) = claim_one_pending(facade, config, now_ns)? else {
        return Ok(None);
    };
    let execution = execute_claimed(config, &claimed);
    finish_claimed(facade, claimed, execution).map(Some)
}

fn validate_process_identity(config: &TriggerCommandConfig) -> Result<(), TriggerError> {
    if config.requester_uid == 0 {
        return Err(TriggerError::new(
            "the per-user trigger runner must not execute as root",
        ));
    }
    // SAFETY: These libc identity accessors have no preconditions.
    let (actual_uid, actual_gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if actual_uid != config.requester_uid || actual_gid != config.requester_gid {
        return Err(TriggerError::new(
            "trigger runner process credentials differ from its configured principal",
        ));
    }
    if !config.jj_executable.is_absolute()
        || !config.daemon_socket.is_absolute()
        || !config.home.is_absolute()
        || config.lease_ns <= 0
    {
        return Err(TriggerError::new(
            "trigger executable, socket, and home must be absolute and its lease positive",
        ));
    }
    Ok(())
}

fn execute_jj(
    config: &TriggerCommandConfig,
    root: &Path,
) -> Result<std::process::ExitStatus, TriggerError> {
    let trigger = OsString::from_vec(b"jj-background-monitor".to_vec());
    Command::new(&config.jj_executable)
        .args(["--quiet", "util", "snapshot"])
        .current_dir(root)
        .env_clear()
        .env("HOME", &config.home)
        .env("WATCHMAN_SOCK", &config.daemon_socket)
        .env("WATCHMAN_ROOT", root)
        .env("WATCHMAN_TRIGGER", trigger)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| TriggerError::context("execute jj snapshot trigger", error))
}

#[derive(Debug)]
pub struct TriggerError {
    message: String,
}

impl TriggerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(context: impl fmt::Display, error: impl fmt::Display) -> Self {
        Self::new(format!("{context}: {error}"))
    }
}

impl fmt::Display for TriggerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TriggerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_runner_refuses_root_and_relative_executables_before_claiming() {
        let config = TriggerCommandConfig {
            jj_executable: PathBuf::from("jj"),
            daemon_socket: PathBuf::from("socket"),
            home: PathBuf::from("home"),
            requester_uid: 0,
            requester_gid: 0,
            run_owner: [1; 16],
            lease_ns: 1,
        };
        assert!(validate_process_identity(&config).is_err());
    }

    #[test]
    fn claimed_trigger_executes_the_fixed_command_without_facade_state() {
        // The production runner deliberately refuses root. Some hermetic test
        // environments execute the whole suite as root, where this positive
        // subprocess test is inapplicable.
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            return;
        }
        let gid = unsafe { libc::getegid() };
        let temp = tempfile::tempdir().unwrap();
        let config = TriggerCommandConfig {
            jj_executable: PathBuf::from("/bin/true"),
            daemon_socket: temp.path().join("watchman.sock"),
            home: temp.path().to_path_buf(),
            requester_uid: uid,
            requester_gid: gid,
            run_owner: [7; 16],
            lease_ns: 1,
        };
        let claimed = ClaimedTrigger {
            run: TriggerRun {
                watch_id: [1; 16],
                authorization_id: [2; 16],
                through_sequence: 3,
                run_owner: [7; 16],
                run_fence: 4,
            },
            root: temp.path().to_path_buf(),
        };
        assert!(execute_claimed(&config, &claimed).unwrap().success());
    }
}
