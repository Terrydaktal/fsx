use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let git_sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=FSX_GIT_SHA={git_sha}");

    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false);
    println!("cargo:rustc-env=FSX_GIT_DIRTY={dirty}");
    println!(
        "cargo:rustc-env=FSX_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned())
    );
    println!(
        "cargo:rustc-env=FSX_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned())
    );
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let rustc_version = Command::new(&rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=RUSTC_VERSION={rustc_version}");
}
