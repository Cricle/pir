#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(feature = "desktop")]
fn main() -> anyhow::Result<()> {
    pir_native_shell::desktop::run()
}

#[cfg(not(feature = "desktop"))]
fn main() {
    eprintln!("pir-native-shell was built without the `desktop` feature");
}
