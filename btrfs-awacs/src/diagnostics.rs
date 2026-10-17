//! Low-volume timing for synchronous work on the direct-scan critical path.

use std::time::{Duration, Instant};

// Keep routine scans quiet at the default INFO level, but preserve the exact
// slow operation and its outcome without requiring a debug rerun.
const SLOW_PHASE: Duration = Duration::from_millis(100);

/// Measures a scope that may exit early. This includes failed operations;
/// their error is reported by the caller rather than logging its contents.
pub(crate) struct ScopeTimer {
    phase: &'static str,
    started: Instant,
}

impl ScopeTimer {
    pub(crate) fn start(phase: &'static str) -> Self {
        Self {
            phase,
            started: Instant::now(),
        }
    }
}

impl Drop for ScopeTimer {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if elapsed >= SLOW_PHASE {
            tracing::info!(
                phase = self.phase,
                pid = std::process::id(),
                ancestor_pids = ?ancestor_pids(),
                elapsed_ms = elapsed.as_millis() as u64,
                "AWACS diagnostic scope completed"
            );
        }
    }
}

pub(crate) fn timed<T, E>(
    phase: &'static str,
    action: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let started = Instant::now();
    tracing::debug!(phase, "AWACS diagnostic phase started");
    let result = action();
    record_phase(phase, started.elapsed(), result.is_ok());
    result
}

fn record_phase(phase: &'static str, elapsed: Duration, success: bool) {
    if elapsed >= SLOW_PHASE || !success {
        // Process IDs, rather than command lines or environment variables,
        // allow correlating an AWACS hook with its Git and JJ callers without
        // recording arguments, tokens, or working-tree contents.
        tracing::info!(
            phase,
            pid = std::process::id(),
            ancestor_pids = ?ancestor_pids(),
            elapsed_ms = elapsed.as_millis() as u64,
            success,
            "AWACS diagnostic phase completed"
        );
    } else {
        tracing::debug!(
            phase,
            elapsed_ms = elapsed.as_millis() as u64,
            success = true,
            "AWACS diagnostic phase completed"
        );
    }
}

fn ancestor_pids() -> Vec<u32> {
    let mut pid = unsafe { libc::getppid() } as u32;
    let mut ancestors = Vec::new();
    // The hook may be invoked by Git directly or through several wrappers.
    // A bounded best-effort /proc lookup also works if a parent has exited.
    while pid > 0 && ancestors.len() < 8 && !ancestors.contains(&pid) {
        ancestors.push(pid);
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        let Some(parent) = parse_parent_pid(&stat) else {
            break;
        };
        pid = parent;
    }
    ancestors
}

fn parse_parent_pid(stat: &str) -> Option<u32> {
    // comm is parenthesized and may itself contain spaces or closing parens.
    let (_, after_comm) = stat.rsplit_once(')')?;
    let mut fields = after_comm.split_whitespace();
    fields.next()?; // process state
    fields.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::Arc;

    #[test]
    fn parse_parent_pid_handles_process_names_with_spaces_and_parentheses() {
        assert_eq!(parse_parent_pid("123 (a ) b) S 456 0 0"), Some(456));
        assert_eq!(parse_parent_pid("123 (a) S not-a-pid"), None);
        assert_eq!(parse_parent_pid("missing comm"), None);
    }

    #[test]
    fn timing_preserves_result() {
        assert_eq!(timed("test", || Ok::<_, ()>(42)), Ok(42));
        assert_eq!(timed("test", || Err::<(), _>("failure")), Err("failure"));
    }

    #[test]
    fn info_logs_only_slow_or_failed_phases_without_error_details() {
        let mut output = tempfile::tempfile().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .without_time()
            .json()
            .with_writer(Arc::new(output.try_clone().unwrap()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            record_phase("quick phase", Duration::ZERO, true);
            assert_eq!(
                timed("failed phase", || Err::<(), _>("private error contents")),
                Err("private error contents")
            );
            drop(ScopeTimer {
                phase: "slow statements",
                started: Instant::now() - SLOW_PHASE,
            });
        });
        output.seek(SeekFrom::Start(0)).unwrap();
        let mut text = String::new();
        output.read_to_string(&mut text).unwrap();
        let events = text.lines().collect::<Vec<_>>();
        assert_eq!(events.len(), 2, "{text}");
        assert!(text.contains("\"phase\":\"failed phase\""));
        assert!(text.contains("\"success\":false"));
        assert!(text.contains("\"phase\":\"slow statements\""));
        assert!(text.contains("\"ancestor_pids\":"));
        assert!(!text.contains("private error contents"));
        assert!(!text.contains("quick phase"));
    }
}
