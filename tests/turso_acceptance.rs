//! Fast package checks; the real isolated desktop rehearsal is the Rust example.
//! cargo test --test turso_acceptance
//! cargo run --example turso_acceptance -- --binary target/release/clip-daemon --nested
#[allow(dead_code)]
#[path = "../examples/turso_acceptance.rs"]
mod rehearsal;

use std::{fs, path::Path};

fn package(root: &Path) {
    for dir in ["bin", "share/systemd/user", "share/dbus-1/services"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(root.join("bin/clip-daemon"), "inert; must never execute").unwrap();
    for (source, destination) in [
        (
            "packaging/systemd/clip-daemon.service",
            "share/systemd/user/clip-daemon.service",
        ),
        (
            "packaging/dbus/org.laufan.ClipDaemon.service",
            "share/dbus-1/services/org.laufan.ClipDaemon.service",
        ),
    ] {
        let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(source)).unwrap();
        fs::write(
            root.join(destination),
            text.replace("@out@", root.to_str().unwrap()),
        )
        .unwrap();
    }
}

#[test]
fn package_has_one_collector_and_correct_installed_activation() {
    let root = tempfile::tempdir().unwrap();
    package(root.path());
    assert_eq!(
        rehearsal::inspect_package(root.path()).unwrap(),
        root.path().join("bin/clip-daemon")
    );
    let units: Vec<_> =
        fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/systemd"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
    assert_eq!(units, ["clip-daemon.service"]);
}

#[test]
fn package_rejects_wrong_binary_and_extra_collectors() {
    let root = tempfile::tempdir().unwrap();
    package(root.path());
    let activation = root
        .path()
        .join("share/dbus-1/services/org.laufan.ClipDaemon.service");
    fs::write(
        &activation,
        "[D-BUS Service]\nName=org.laufan.ClipDaemon\nExec=/wrong/bin/clip-daemon daemon\n",
    )
    .unwrap();
    assert!(rehearsal::inspect_package(root.path()).is_err());
    package(root.path());
    fs::write(
        root.path().join("share/systemd/user/extra.service"),
        "[Service]",
    )
    .unwrap();
    assert!(rehearsal::inspect_package(root.path()).is_err());
}

#[test]
fn runtime_has_no_legacy_engine_or_bootstrap_dependency() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "flake.nix",
        "src/lib.rs",
        "src/main.rs",
    ] {
        let text = fs::read_to_string(root.join(name)).unwrap().to_lowercase();
        for forbidden in [
            "clipboard-history-client-sdk",
            "clipboard-history-core",
            "rustc_bootstrap",
            "ringboard-server",
            "pub mod ringboard",
        ] {
            assert!(!text.contains(forbidden), "{name} retains {forbidden}");
        }
    }
}
