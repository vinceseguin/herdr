#![cfg(unix)]
//! `scripts/fork/fleet-lab.sh` boots N isolated named herdr sessions.
//!
//! The lab is the fixture every later fork epic validates against, so this test
//! drives the real script against real servers: it must come up, report a
//! machine-readable status, keep every byte of state inside its own root, and
//! tear itself down without leaving a process or a directory behind.

pub mod support;

use std::path::PathBuf;
use std::time::Duration;

use support::fleet_lab::{stderr_of, stdout_of, Lab};

const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

fn status_json(lab: &Lab) -> serde_json::Value {
    let output = lab.run(&["status", "--json"]);
    assert!(
        output.status.success(),
        "status --json failed: {}",
        stderr_of(&output)
    );
    serde_json::from_str(&stdout_of(&output)).expect("status --json emits one JSON object")
}

#[test]
fn fleet_lab_boots_isolated_sessions_and_tears_them_down() {
    let mut lab = Lab::new("life");

    let up = lab.up("2");
    assert!(
        up.status.success(),
        "up 2 failed: {}{}",
        stdout_of(&up),
        stderr_of(&up)
    );

    let status = status_json(&lab);
    assert_eq!(
        status["root"].as_str(),
        Some(lab.root.to_string_lossy().as_ref()),
        "status reports the lab root"
    );
    let sessions = status["sessions"]
        .as_array()
        .expect("status --json carries a sessions array")
        .clone();
    assert_eq!(sessions.len(), 2, "status: {status}");

    for (index, session) in sessions.iter().enumerate() {
        let expected_name = format!("lab-{}", index + 1);
        assert_eq!(session["name"].as_str(), Some(expected_name.as_str()));
        assert_eq!(session["running"].as_bool(), Some(true), "{session}");
        assert!(session["pid"].as_u64().is_some(), "{session}");
        assert!(session["pane_id"].as_str().is_some(), "{session}");

        let client_socket = PathBuf::from(
            session["client_socket"]
                .as_str()
                .expect("client_socket path"),
        );
        assert!(
            client_socket.starts_with(&lab.root),
            "every socket stays inside the lab root: {}",
            client_socket.display()
        );
        support::wait_for_socket(&client_socket, SOCKET_TIMEOUT);
    }

    // The pane of lab-2 really runs the lab's marker process.
    let pane_id = sessions[1]["pane_id"]
        .as_str()
        .expect("lab-2 pane id")
        .to_string();
    let read = lab.herdr("lab-2", &["pane", "read", &pane_id, "--source", "recent"]);
    assert!(read.status.success(), "pane read: {}", stderr_of(&read));
    assert!(
        stdout_of(&read).contains("herdr-fleet-lab:lab-2"),
        "pane read did not show the lab marker: {}",
        stdout_of(&read)
    );

    // `env` is a published output shape: later epics eval it.
    let env_output = lab.run(&["env"]);
    assert!(env_output.status.success(), "{}", stderr_of(&env_output));
    let env_text = stdout_of(&env_output);
    assert!(
        env_text.contains(r#"export HERDR_FLEET_LAB_SESSIONS="lab-1 lab-2""#),
        "env output: {env_text}"
    );
    assert!(
        env_text.contains("export HERDR_FLEET_LAB_CLIENT_SOCKET_2="),
        "env output: {env_text}"
    );
    assert!(
        env_text.contains(&format!(
            r#"export XDG_CONFIG_HOME="{}/xdg""#,
            lab.root.display()
        )),
        "env output: {env_text}"
    );

    // A second `up` must refuse rather than start a second set of servers.
    let again = lab.run(&["up", "2"]);
    assert!(!again.status.success(), "second up should fail");
    assert!(
        stderr_of(&again).contains("already up"),
        "second up stderr: {}",
        stderr_of(&again)
    );

    let down = lab.run(&["down"]);
    assert!(down.status.success(), "down: {}", stderr_of(&down));
    assert!(!lab.root.exists(), "down removed the lab root");

    #[cfg(target_os = "linux")]
    {
        let leaked = support::herdr_server_pids_for_runtime_dir(&lab.runtime_dir())
            .expect("scan for leaked lab servers");
        assert!(leaked.is_empty(), "down left servers running: {leaked:?}");
    }

    // Tearing down a lab that is already gone is a no-op, not an error.
    let down_again = lab.run(&["down"]);
    assert!(
        down_again.status.success(),
        "second down: {}",
        stderr_of(&down_again)
    );
}

#[test]
fn fleet_lab_up_fails_without_leaving_a_root_when_the_binary_is_missing() {
    let lab = Lab::new("nobin");
    let missing = lab.root.join("nonexistent-herdr");

    let output = lab.run_with_bin(&missing.to_string_lossy(), &["up", "1"]);

    assert!(
        !output.status.success(),
        "up with a missing binary must fail"
    );
    assert!(
        stderr_of(&output).contains("herdr binary not found"),
        "up stderr: {}",
        stderr_of(&output)
    );
    assert!(
        !lab.root.exists(),
        "a failed up must not leave {} behind",
        lab.root.display()
    );
}

#[test]
fn fleet_lab_up_cleans_up_when_the_server_exits_early() {
    let lab = Lab::new("early");

    // `false` accepts any argv and exits at once: the lab's server dies before
    // it ever becomes ready, so `up` must report that and undo its own root.
    let output = lab.run_with_bin("/bin/false", &["up", "1"]);

    assert!(
        !output.status.success(),
        "up with a server that exits early must fail"
    );
    assert!(
        stderr_of(&output).contains("exited early"),
        "up stderr: {}",
        stderr_of(&output)
    );
    assert!(
        !lab.root.exists(),
        "a failed up must not leave {} behind",
        lab.root.display()
    );
}

#[test]
fn fleet_lab_refuses_a_root_without_its_marker() {
    let lab = Lab::new("nomark");
    let decoy = lab.root.join("decoy");
    std::fs::create_dir_all(&decoy).expect("create decoy root");
    let marker = lab.root.join(".herdr-fleet-lab");

    let assert_refused = |what: &str| {
        let up = lab.run(&["up", "1"]);
        assert!(!up.status.success(), "up must refuse a root {what}");
        assert!(
            stderr_of(&up).contains("marker"),
            "up stderr ({what}): {}",
            stderr_of(&up)
        );

        let down = lab.run(&["down"]);
        assert!(!down.status.success(), "down must refuse a root {what}");
        assert!(
            stderr_of(&down).contains("marker"),
            "down stderr ({what}): {}",
            stderr_of(&down)
        );
        assert!(
            decoy.exists(),
            "down must not delete a directory it does not own ({what})"
        );
    };

    assert_refused("without a marker");

    std::fs::create_dir(&marker).expect("marker directory");
    assert_refused("whose marker is a directory");
    std::fs::remove_dir(&marker).expect("remove marker directory");

    let elsewhere = lab.root.join("decoy/looks-like-a-marker");
    std::fs::write(&elsewhere, "herdr-fleet-lab\nbin=/x\n").expect("fake marker");
    std::os::unix::fs::symlink(&elsewhere, &marker).expect("symlink marker");
    assert_refused("whose marker is a symlink");
    std::fs::remove_file(&marker).expect("remove marker symlink");

    std::fs::write(&marker, "not-a-fleet-lab\n").expect("foreign marker");
    assert_refused("whose marker was not written by the lab");
}
