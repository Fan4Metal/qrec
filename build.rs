//! Generates the application icon and embeds it (plus version info) into the
//! Windows executable, and passes the version shown in the UI to the program.

#[path = "src/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=QREC_VERSION={}", version());

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out_dir.join("app.ico");
    std::fs::write(&ico_path, icon::ico(&icon::ICO_SIZES)).expect("write app.ico");
    // The icon as raw RGBA for the window icon (see `main`), so that
    // nothing is rasterised at start-up.
    std::fs::write(out_dir.join("app_icon_64.rgba"), icon::rgba(64)).expect("write the window icon");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().unwrap())
        .set("FileDescription", "qrec - screen area recorder")
        .set("ProductName", "qrec")
        .set("LegalCopyright", "Copyright (C) 2026 Fan4_Metal");
    if let Err(e) = res.compile() {
        // A missing resource compiler must not break the build.
        println!("cargo:warning=icon not embedded: {e}");
    }
}

/// The version shown in the UI: Cargo's, plus the commit for a development
/// version ("0.1.0-dev (v0.1.0-2-g4bd494d)"), so that builds between
/// releases can be told apart. A release version is shown as is.
fn version() -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    if !version.contains('-') {
        return version;
    }
    // `--always` falls back to the abbreviated hash when no tag is reachable
    // (a shallow checkout fetches none). No `--dirty`: this script is not rerun
    // when only sources change, so the mark would be stale.
    match git(&["describe", "--tags", "--always"]) {
        Some(describe) => {
            watch_git();
            format!("{version} ({describe})")
        }
        None => version,
    }
}

/// Reruns this script when a commit is made, the branch is switched or a tag
/// is added: HEAD, the branch it points to, packed refs and the tags.
fn watch_git() {
    let Some(dirs) = git(&["rev-parse", "--git-dir", "--git-common-dir"]) else { return };
    let mut dirs = dirs.lines().map(std::path::PathBuf::from);
    let (Some(git_dir), Some(common)) = (dirs.next(), dirs.next()) else { return };
    let head = git_dir.join("HEAD");
    let mut paths = vec![head.clone(), common.join("packed-refs"), common.join("refs").join("tags")];
    if let Some(branch) = std::fs::read_to_string(&head).ok().and_then(|h| h.strip_prefix("ref: ").map(|r| r.trim().to_owned())) {
        paths.push(common.join(branch));
    }
    // A path that does not exist would make Cargo rerun the script every time.
    for path in paths.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Output of a git command, trimmed; `None` when git is missing or fails.
fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}
