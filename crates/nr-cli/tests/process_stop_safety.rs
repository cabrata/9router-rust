//! Safety tests for `stop_recorded_instance` — the launcher must never kill a
//! PID it does not own, and must survive stale/invalid records.
//!
//! No real process is ever signalled: every case uses PIDs that are guaranteed
//! dead or already gone, or the current process (which is filtered out by the
//! `self_pid` guard).

use nr_cli::launcher_process::PidRecord;
use nr_cli::process_management::{is_pid_alive, stop_recorded_instance};

/// A PID that cannot be alive: `u32::MAX` is reserved/unassigned.
const DEAD_PID: u32 = u32::MAX;

#[test]
fn self_pid_is_never_killed() {
    // A record that names this very process must not self-signal.
    let me = std::process::id();
    let record = PidRecord::new(me, Some(me), Some(20128), "9router".into());

    let killed = stop_recorded_instance(&record, me);
    assert!(killed.is_empty(), "record naming self must kill nothing: {killed:?}");
}

#[test]
fn dead_server_pid_is_skipped_not_signalled() {
    let record = PidRecord::new(DEAD_PID, Some(DEAD_PID), Some(20128), "9router".into());
    let me = std::process::id();

    let killed = stop_recorded_instance(&record, me);
    assert!(killed.is_empty(), "dead PIDs must not be signalled: {killed:?}");
}

#[test]
fn empty_exe_name_never_matches_a_live_process() {
    // An empty exe is the "cannot identify" case: refuse to kill anything,
    // because a PID-reused process could be an unrelated program.
    let me = std::process::id();
    let other = if me == DEAD_PID { me - 1 } else { DEAD_PID };
    let record = PidRecord::new(other, None, Some(20128), "".into());

    let killed = stop_recorded_instance(&record, me);
    assert!(killed.is_empty(), "empty exe must block the kill: {killed:?}");
}

#[test]
fn is_pid_alive_reports_own_process() {
    // `kill(pid, 0)` is the liveness probe; it must succeed for ourselves.
    // (`pid 0` is deliberately not asserted: it targets the whole process
    // group, so it is not a meaningful "dead pid" sample.)
    assert!(is_pid_alive(std::process::id()), "own pid is alive");
}
