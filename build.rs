//! Embeds the app icon and version info into the Windows executable, and the git commit
//! (`EDITTR_COMMIT`, empty outside a checkout) shown in Settings > About.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let commit = git(&["rev-parse", "--short", "HEAD"]);
    let date = git(&["show", "-s", "--format=%cs", "HEAD"]);
    println!("cargo:rustc-env=EDITTR_COMMIT={commit}");
    println!("cargo:rustc-env=EDITTR_COMMIT_DATE={date}");
    // Rebuild when HEAD moves: HEAD itself (branch switch) and the branch it points to.
    let git_dir = git(&["rev-parse", "--git-dir"]);
    if !git_dir.is_empty() {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        let branch = git(&["symbolic-ref", "-q", "HEAD"]);
        if !branch.is_empty() {
            println!("cargo:rerun-if-changed={git_dir}/{branch}");
        }
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "edittr code editor");
        res.set("ProductName", "edittr");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed Windows icon: {e}");
        }
    }
}
