use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn main() {
    // e.g. "6c9c22e" or "6c9c22e-dirty" when there are uncommitted changes
    let revision = run("git", &["describe", "--always", "--dirty"])
        .unwrap_or_else(|| "unknown".to_string());
    let build_time =
        run("date", &["+%Y-%m-%d %H:%M:%S"]).unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=IVYTERM_GIT_REVISION={}", revision);
    println!("cargo:rustc-env=IVYTERM_BUILD_TIME={}", build_time);

    // Re-run on any source change (not just commits) so the embedded
    // build time always matches the binary
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
