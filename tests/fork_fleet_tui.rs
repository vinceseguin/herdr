#![cfg(unix)]
//! The Fleet console against real herdr servers.
//!
//! `herdr fleet` opens the client-owned shell over every configured host. This
//! proves the two things a console has to get right before anything is drawn
//! for a second host: the active host's screen is what you see, and what you
//! type reaches that machine and no other.
//!
//! Every check drives the real binary through a PTY against
//! `scripts/fork/fleet-lab.sh` sessions, in the lab's own throwaway
//! `XDG_CONFIG_HOME` — never the developer's herdr.

pub mod support;

use std::process::Command;
use std::time::Duration;

use support::fleet_lab::{stderr_of, stdout_of, Lab};
use support::fleet_tui::{
    append_lab_config, assert_screen, lab_client_socket, lab_fleet_config, lab_pane_id, pane_text,
    FleetConsole,
};

const COLS: u16 = 120;
const ROWS: u16 = 40;
const RENDER_TIMEOUT: Duration = Duration::from_secs(30);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn fleet_console_shows_the_active_host_and_routes_input_to_it() {
    let mut lab = Lab::new("tui");
    let up = lab.up("2");
    assert!(
        up.status.success(),
        "fleet-lab up 2 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );
    for session in ["lab-1", "lab-2"] {
        support::wait_for_socket(&lab_client_socket(&lab, session), SOCKET_TIMEOUT);
    }
    let pane_1 = lab_pane_id(&lab, 1);
    let pane_2 = lab_pane_id(&lab, 2);
    append_lab_config(&lab, &lab_fleet_config(2));

    let servers_before = support::herdr_server_pids_for_runtime_dir(&lab.runtime_dir())
        .expect("list the lab's servers");

    let mut console = FleetConsole::spawn(&lab, COLS, ROWS);

    // The first configured host is the active one, and its pane is what the
    // console renders: the lab's marker pane prints its own session name.
    assert_screen(
        &console,
        "herdr-fleet-lab:lab-1",
        RENDER_TIMEOUT,
        "the active host's pane never rendered",
    );

    console.send(b"echo tui-1\r");
    assert!(
        support::wait_until(Duration::from_secs(20), Duration::from_millis(200), || {
            pane_text(&lab, "lab-1", &pane_1).contains("tui-1")
        }),
        "the keystrokes never reached lab-1:\n{}",
        pane_text(&lab, "lab-1", &pane_1)
    );
    assert!(
        !pane_text(&lab, "lab-2", &pane_2).contains("tui-1"),
        "input reached a machine the console was not showing:\n{}",
        pane_text(&lab, "lab-2", &pane_2)
    );

    // The console starts no server of its own: it is a client of the two the
    // lab booted, and nothing else.
    let servers_during = support::herdr_server_pids_for_runtime_dir(&lab.runtime_dir())
        .expect("list the lab's servers");
    assert_eq!(
        servers_during, servers_before,
        "the console must not install, start or hand off a herdr"
    );

    assert!(
        console.detach(EXIT_TIMEOUT),
        "prefix+q did not end the console:\n{}",
        console.screen_text()
    );
}

#[test]
fn a_fleet_with_no_enabled_host_refuses_before_it_touches_the_terminal() {
    let mut lab = Lab::new("tui-empty");
    let up = lab.up("1");
    assert!(up.status.success(), "fleet-lab up 1: {}", stderr_of(&up));
    append_lab_config(&lab, "\n[fleet]\ninclude_local = false\n");

    let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .arg("fleet")
        .env("XDG_CONFIG_HOME", lab.root.join("xdg"))
        .env("XDG_RUNTIME_DIR", lab.runtime_dir())
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SESSION")
        .output()
        .expect("run herdr fleet");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no fleet hosts enabled"),
        "the console says what to fix: {stderr}"
    );
}

/// `--fleet` is the console's flag alias. The nested guard is the cheapest
/// proof that the flag was accepted *and* reached the console's launch branch,
/// without opening a terminal in the test harness.
#[test]
fn the_fleet_flag_is_accepted_and_launches_the_console() {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .arg("--fleet")
        .env("HERDR_ENV", "1")
        .output()
        .expect("run herdr --fleet");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unknown option"),
        "--fleet is a known flag: {stderr}"
    );
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("nested herdr is disabled by default"),
        "the flag reached the console launch branch: {stderr}"
    );
}

#[test]
fn the_fleet_flag_cannot_be_combined_with_remote() {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(["--fleet", "--remote", "example.invalid"])
        .env("HERDR_ENV", "1")
        .output()
        .expect("run herdr --fleet --remote");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--remote can only be used with the default launch command"),
        "stderr: {stderr}"
    );
}
