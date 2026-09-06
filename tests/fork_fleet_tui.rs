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

/// SGR mouse press and release at a 1-based screen position.
fn sgr_click(column: u16, row: u16) -> Vec<u8> {
    format!("\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m").into_bytes()
}

/// Screen row of the second host's header, on a console showing the first.
///
/// The spaces list starts two rows under the sidebar's " spaces" title, and
/// each lab session has exactly one workspace, so the rows are: the first
/// host's header, its workspace, then the second host's header. If the sidebar
/// layout ever moves, the assertions below fail with the screen attached.
const SECOND_HOST_HEADER_ROW: u16 = 5;
/// Column 3: past the `▾`/`▸` collapse cell, inside the header's text.
const HEADER_COLUMN: u16 = 3;

#[test]
fn sidebar_lists_every_host_and_click_switches() {
    let mut lab = Lab::new("tui-hosts");
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

    let mut console = FleetConsole::spawn(&lab, COLS, ROWS);

    // Both hosts have a group. lab-2's rows are drawn from the fleet model —
    // the console has never shown that machine.
    assert_screen(
        &console,
        "▾ lab-1",
        RENDER_TIMEOUT,
        "the active host's group",
    );
    assert_screen(
        &console,
        "▾ lab-2",
        RENDER_TIMEOUT,
        "the inactive host's group is listed too",
    );
    assert_screen(
        &console,
        "herdr-fleet-lab:lab-1",
        RENDER_TIMEOUT,
        "the active host's pane never rendered",
    );
    // lab-2's *workspace* row only exists once lab-2 sent a projection, so this
    // is the console saying that host is connected. Switching earlier is
    // allowed and routes correctly, but what is typed while the target is
    // still connecting is dropped rather than queued, which would make the
    // marker below a race.
    assert_screen(
        &console,
        "· lab-2",
        RENDER_TIMEOUT,
        "lab-2 never reported a projection",
    );

    // Click lab-2's header: one real SGR press through the real hit map.
    console.send(&sgr_click(HEADER_COLUMN, SECOND_HOST_HEADER_ROW));
    console.send(b"switch-marker");

    // The console draws the *new* host, so the echo of what was typed can only
    // be on screen if the pane area followed the switch. (A client draws frame
    // diffs, so asserting on lab-2's pre-existing text would assert on bytes
    // that never had to be redrawn.)
    assert_screen(
        &console,
        "switch-marker",
        RENDER_TIMEOUT,
        "the console never showed the host it switched to",
    );
    assert!(
        support::wait_until(Duration::from_secs(20), Duration::from_millis(200), || {
            pane_text(&lab, "lab-2", &pane_2).contains("switch-marker")
        }),
        "input after the switch never reached lab-2:\n{}",
        pane_text(&lab, "lab-2", &pane_2)
    );
    // The failure this PR is shaped around: typing into the machine the console
    // just left.
    assert!(
        !pane_text(&lab, "lab-1", &pane_1).contains("switch-marker"),
        "input after the switch reached the previous host:\n{}",
        pane_text(&lab, "lab-1", &pane_1)
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

/// A throwaway, empty config home for a launch that must exit before it
/// touches a terminal or a server. Never the developer's `~/.config/herdr*`:
/// the nested guard below reads `[experimental] allow_nested`, and a test
/// must not depend on — or log into — the real one.
fn scratch_config_home(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "herdr-fleet-tui-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("scratch config home");
    dir
}

fn bare_launch(args: &[&str], config_home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(args)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", config_home)
        .env("HERDR_ENV", "1")
        .env_remove("HERDR_CONFIG_PATH")
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_SESSION")
        .output()
        .expect("run herdr")
}

/// `--fleet` is the console's flag alias. The nested guard is the cheapest
/// proof that the flag was accepted *and* reached the console's launch branch,
/// without opening a terminal in the test harness.
#[test]
fn the_fleet_flag_is_accepted_and_launches_the_console() {
    let config_home = scratch_config_home("flag");
    let output = bare_launch(&["--fleet"], &config_home);
    let _ = std::fs::remove_dir_all(&config_home);

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
    let config_home = scratch_config_home("flag-remote");
    let output = bare_launch(&["--fleet", "--remote", "example.invalid"], &config_home);
    let _ = std::fs::remove_dir_all(&config_home);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--remote can only be used with the default launch command"),
        "stderr: {stderr}"
    );
}
