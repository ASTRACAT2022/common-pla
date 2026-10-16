#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args().nth(1).as_deref() == Some("--privileged-helper") {
        std::process::exit(commonnet_desktop_lib::run_linux_privileged_helper());
    }
    commonnet_desktop_lib::run();
}
