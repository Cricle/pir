//! End-to-end coverage for the CLI application boundary.

use std::process::Command;

#[test]
fn cli_print_and_version_are_end_to_end_boundaries() {
    let home = std::env::temp_dir().join(format!("pir-e2e-cli-no-provider-{}", std::process::id()));
    let mut print = Command::new(env!("CARGO_BIN_EXE_pir"));
    print
        .args(["print", "e2e prompt"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        // Empty agent dir: no auth.json and no provider env credentials.
        .env("PIR_AGENT_DIR", &home);
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            print.env_remove(key);
        }
    }
    let print = print.output().expect("pir print should start");
    assert!(!print.status.success());
    assert!(String::from_utf8_lossy(&print.stderr).contains("no model available"));

    let version = Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg("version")
        .output()
        .expect("pir version should start");
    assert!(version.status.success());
    let text = String::from_utf8_lossy(&version.stdout);
    assert!(text.contains("Protocol version: 8"));
}
