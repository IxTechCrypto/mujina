use std::path::PathBuf;

fn main() {
    // Short git sha for GET /api/v0/system (empty when git is unavailable).
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    println!("cargo:rustc-env=MUJINA_GIT_SHA={sha}");
    // Re-run when HEAD moves: watch HEAD and the ref it points to, plus
    // packed-refs. `git rev-parse --git-path` resolves worktrees correctly.
    let git_path = |p: &str| {
        std::process::Command::new("git")
            .args(["rev-parse", "--git-path", p])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    for p in ["HEAD", "packed-refs"] {
        if let Some(path) = git_path(p) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Ok(o) = std::process::Command::new("git").args(["symbolic-ref", "-q", "HEAD"]).output()
        && o.status.success()
        && let Some(path) = git_path(String::from_utf8_lossy(&o.stdout).trim())
    {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_NANO3S");
    if std::env::var("CARGO_FEATURE_NANO3S").is_err() {
        return;
    }

    // rtos_core lives two levels up from this crate
    // (mujina-upstream/mujina-miner -> mujina-upstream -> nano3s -> rtos_core).
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let rtos_core = manifest_dir
        .parent()
        .expect("mujina-miner has a parent dir")
        .parent()
        .expect("mujina-upstream has a parent dir")
        .join("rtos_core");

    let tools = rtos_core.join("tools");
    let include = rtos_core.join("include");
    let sdk_libs = rtos_core.join("vendor").join("sdk_libs");

    println!("cargo:rerun-if-changed={}", tools.join("nano3s_ipc_shim.c").display());
    println!("cargo:rerun-if-changed={}", tools.join("nano3s_ipc_shim.h").display());
    println!("cargo:rerun-if-changed={}", rtos_core.join("src").join("ipc_protocol.c").display());
    println!("cargo:rerun-if-changed={}", include.join("ipc_protocol.h").display());

    cc::Build::new()
        .file(tools.join("nano3s_ipc_shim.c"))
        .file(rtos_core.join("src").join("ipc_protocol.c"))
        .include(&include)
        .warnings(true)
        .compile("nano3s_ipc_shim");

    println!("cargo:rustc-link-search=native={}", sdk_libs.display());
    println!("cargo:rustc-link-lib=static=ipcmsg");
}
