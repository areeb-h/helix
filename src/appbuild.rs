//! `helix build --app` (ADR 0063 D3, D4): a native app whose permissions come from its code.
//!
//! Two things no other app toolchain does together:
//!
//! - **The permissions are inferred, not declared.** The same closure `helix effects` prints
//!   — which files, network and programs each function can reach — is taken over the app's
//!   own code, and the union becomes the app's capability ceiling, baked into the executable
//!   and enforced by the ADR 0021 gate. So the manifest a user or a reviewer reads cannot
//!   disagree with what the app does: the manifest IS what the app does. A reach the
//!   analysis could not follow is named at build time, and if it is ever taken the gate
//!   refuses it loudly instead of allowing it.
//! - **One command, the platform's own shape.** An AppDir with a `.desktop` file on Linux, a
//!   `.app` bundle with an entitlements file on macOS, an `.exe` on Windows — no Xcode
//!   project, Gradle file or `.csproj`.
//!
//! A built app runs in its own window (no port, ADR 0063 D1). The runtime's browser fallback
//! — `listen` and `open_url` — is therefore not part of what it reaches, and is left out of
//! the inference; a built app with no display says so instead of falling back.

use std::collections::BTreeSet;

use crate::ast::{Expr, Stmt};
use crate::capability::Effect;
use crate::module::Span;
use crate::pkg::Capabilities;

/// What the app's code reaches, and why.
pub struct Inferred {
    pub caps: Capabilities,
    /// Each category granted, with the call path that needs it (`update -> save -> write_text`).
    pub grants: Vec<(Effect, Vec<String>)>,
    /// Calls the analysis could not resolve, with where — named at build time.
    pub unresolved: Vec<(String, Vec<String>)>,
    /// Categories the app reaches that a built app cannot be granted (environment variables).
    pub refused: Vec<(Effect, Vec<String>)>,
}

/// The library's runtime functions whose reach is not the app's in a built app:
/// - `serve_tcp` is the browser fallback (`listen`, `open_url`), which a built app never takes;
/// - `serve` is the event loop. Its connection verbs (`respond`, `send`, …) look like network
///   use to the analysis, but in a built app they act on the window's own link, which reaches
///   nothing but the app's page and is not behind the `net` gate (ADR 0063 D1). What the loop
///   calls of the APP — `update`, `view` — is counted from the app's own functions.
const FALLBACK_ONLY: &[&str] = &["serve_tcp", "serve"];

/// A function's name as written: the merged program prefixes a module's functions (`m0$`) and
/// suffixes a name that collided (`$263`), and neither means anything to a reader.
pub fn base_name(name: &str) -> &str {
    let mut n = name;
    if let Some(rest) = n.strip_prefix('m')
        && let Some((digits, after)) = rest.split_once('$')
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        n = after;
    }
    if let Some((head, tail)) = n.rsplit_once('$')
        && !tail.is_empty()
        && tail.bytes().all(|b| b.is_ascii_digit())
    {
        n = head;
    }
    n
}

/// A call path as a person reads it: `update -> save -> write_text`.
pub fn path_text(path: &[String]) -> String {
    path.iter().map(|p| base_name(p)).collect::<Vec<_>>().join(" -> ")
}

/// Whether `line` (of the merged program) belongs to the app's own files, not the library.
fn is_user_line(spans: &[Span], line: usize) -> bool {
    let (_, filename, _) = crate::module::locate(spans, line);
    !filename.starts_with(crate::module::EMBED_ROOT)
}

/// Infer the app's capability ceiling from its loaded program.
pub fn infer(stmts: &[Stmt], spans: &[Span]) -> Inferred {
    let mut user: BTreeSet<String> = BTreeSet::new();
    let mut program: Vec<Stmt> = Vec::with_capacity(stmts.len() + 1);
    let mut top: Vec<(String, Expr)> = Vec::new();
    for s in stmts {
        match s {
            Stmt::Func { name, line, .. } if is_user_line(spans, *line) => {
                user.insert(name.clone());
                program.push(s.clone());
            }
            Stmt::Func { name, line, body, .. }
                if !is_user_line(spans, *line) && FALLBACK_ONLY.contains(&base_name(name)) =>
            {
                // Kept as a function (so callers still resolve) that reaches nothing.
                let _ = body;
                let mut stub = s.clone();
                if let Stmt::Func { body, .. } = &mut stub {
                    *body = Expr::Missing;
                }
                program.push(stub);
            }
            Stmt::Assign { name, value: Expr::Lambda { .. }, line, .. } if is_user_line(spans, *line) => {
                user.insert(name.clone());
                program.push(s.clone());
            }
            // The app's top-level statements run too: gathered into one function below.
            Stmt::Assign { name, value, line, .. } if is_user_line(spans, *line) => {
                top.push((name.clone(), value.clone()));
                program.push(s.clone());
            }
            Stmt::Expr(e) if is_user_line(spans, stmt_line(s)) => {
                top.push((format!("$top{}", top.len()), e.clone()));
                program.push(s.clone());
            }
            _ => program.push(s.clone()),
        }
    }
    const TOP: &str = "(top level)";
    program.push(Stmt::Func {
        name: TOP.into(),
        params: Vec::new(),
        defaults: Vec::new(),
        ret: None,
        exported: false,
        body: Expr::Let { bindings: top, body: Box::new(Expr::Missing), from_do: true },
        line: 0,
        col: 0,
    });
    user.insert(TOP.into());

    let mut grants: Vec<(Effect, Vec<String>)> = Vec::new();
    let mut refused: Vec<(Effect, Vec<String>)> = Vec::new();
    let mut unresolved: Vec<(String, Vec<String>)> = Vec::new();
    for f in crate::effects::closure(&program) {
        if !user.contains(&f.name) {
            continue;
        }
        for reach in [&f.does, &f.carries] {
            for (e, path) in &reach.effects {
                let into = if *e == Effect::Env { &mut refused } else { &mut grants };
                if !into.iter().any(|(have, _)| have == e) {
                    into.push((*e, path.clone()));
                }
            }
            // Only a call in the app's own code is worth a note: the library's unresolved
            // calls are to the app's functions (`spec.update`), which are counted above.
            let in_user_code = |path: &Vec<String>| path.len() < 2 || user.contains(&path[path.len() - 2]);
            if let Some((what, path)) = &reach.unknown
                && in_user_code(path)
                && !unresolved.iter().any(|(w, _)| w == what)
            {
                unresolved.push((what.clone(), path.clone()));
            }
        }
    }
    grants.sort_by_key(|(e, _)| format!("{e:?}"));
    let has = |e: Effect| grants.iter().any(|(g, _)| *g == e);
    let fs = match (has(Effect::FsRead), has(Effect::FsWrite)) {
        (true, true) => Some("all".to_string()),
        (true, false) => Some("read".to_string()),
        (false, true) => Some("write".to_string()),
        _ => None,
    };
    // A database write reaches the far end over the network, so it needs `net` as well.
    let net = (has(Effect::Net) || has(Effect::DbWrite)).then(|| "on".to_string());
    let caps = Capabilities {
        fs,
        net,
        process: has(Effect::Process).then(|| "on".to_string()),
        db: has(Effect::DbWrite).then(|| "write".to_string()),
    };
    Inferred { caps, grants, unresolved, refused }
}

fn stmt_line(s: &Stmt) -> usize {
    match s {
        Stmt::Expr(e) => e.position().0,
        Stmt::Assign { line, .. } | Stmt::Func { line, .. } => *line,
        _ => 0,
    }
}

/// How a category is said to a person.
pub fn plain(e: Effect) -> &'static str {
    match e {
        Effect::FsRead => "read files",
        Effect::FsWrite => "write files",
        Effect::Net => "use the network",
        Effect::Process => "run programs",
        Effect::DbWrite => "write to databases",
        Effect::Env => "read environment variables",
        Effect::Pure => "nothing",
    }
}

/// The manifest that travels with the app: what it may do, and the code that needs each.
pub fn manifest(name: &str, inf: &Inferred) -> String {
    let grants: Vec<serde_json::Value> = inf
        .grants
        .iter()
        .map(|(e, path)| serde_json::json!({"may": plain(*e), "because": path_text(path)}))
        .collect();
    let unresolved: Vec<serde_json::Value> =
        inf.unresolved.iter().map(|(w, path)| serde_json::json!({"call": w, "at": path_text(path)})).collect();
    let doc = serde_json::json!({
        "app": name,
        "built_with": format!("helix {}", env!("CARGO_PKG_VERSION")),
        "inferred_from_code": true,
        "enforced": true,
        "network_port": false,
        "capabilities": inf.caps,
        "grants": grants,
        "not_followed": unresolved,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// One file of a bundle: where it goes, its bytes, and whether it is executable.
pub struct Entry {
    pub path: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// A name safe as a file name and an identifier: letters, digits, `-` and `_`.
pub fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "app".into() } else { s }
}

/// A display name from a file stem: `habit_tracker` is `Habit Tracker`.
pub fn display_name(stem: &str) -> String {
    stem.split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The app's icon: its initial on a rounded square. A real icon can replace the file.
fn icon_svg(display: &str) -> String {
    let initial = display.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "H".into());
    let initial = initial.replace('&', "&amp;").replace('<', "&lt;");
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 256 256\">\
         <defs><linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\"><stop offset=\"0\" stop-color=\"#6d5dfc\"/><stop offset=\"1\" stop-color=\"#3b2fd1\"/></linearGradient></defs>\
         <rect width=\"256\" height=\"256\" rx=\"56\" fill=\"url(#g)\"/>\
         <text x=\"128\" y=\"170\" font-family=\"system-ui, sans-serif\" font-size=\"136\" font-weight=\"700\" text-anchor=\"middle\" fill=\"#fff\">{initial}</text></svg>\n"
    )
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The files of the app's bundle for `os` ("linux", "macos" or "windows"), relative to the
/// output directory. `exe` is the built executable.
pub fn layout(os: &str, display: &str, exe: Vec<u8>, inf: &Inferred, version: &str) -> Vec<Entry> {
    let id = slug(display);
    let manifest = manifest(display, inf).into_bytes();
    let says: Vec<&str> = inf.grants.iter().map(|(e, _)| plain(*e)).collect();
    let summary = if says.is_empty() { "none".to_string() } else { says.join(", ") };
    match os {
        "macos" => {
            let root = format!("{display}.app/Contents");
            let plist = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <plist version=\"1.0\"><dict>\n\
                 <key>CFBundleName</key><string>{d}</string>\n\
                 <key>CFBundleDisplayName</key><string>{d}</string>\n\
                 <key>CFBundleIdentifier</key><string>app.helix.{id}</string>\n\
                 <key>CFBundleExecutable</key><string>{id}</string>\n\
                 <key>CFBundlePackageType</key><string>APPL</string>\n\
                 <key>CFBundleShortVersionString</key><string>{v}</string>\n\
                 <key>CFBundleVersion</key><string>{v}</string>\n\
                 <key>LSMinimumSystemVersion</key><string>11.0</string>\n\
                 <key>NSHighResolutionCapable</key><true/>\n\
                 </dict></plist>\n",
                d = xml(display),
                v = xml(version),
            );
            // The App Sandbox entitlements, from the same inference. A child process cannot
            // run inside the sandbox, so an app that runs programs is built without it — and
            // the file says so rather than claiming a sandbox it would break.
            let ent = |k: &str| format!("<key>{k}</key><true/>\n");
            let mut body = String::new();
            if inf.caps.process_on() {
                body.push_str("<!-- This app runs programs, which the App Sandbox forbids; it is not sandboxed. -->\n");
            } else {
                body.push_str(&ent("com.apple.security.app-sandbox"));
                if inf.caps.net_on() {
                    body.push_str(&ent("com.apple.security.network.client"));
                }
                if inf.caps.fs_write() {
                    body.push_str(&ent("com.apple.security.files.user-selected.read-write"));
                } else if inf.caps.fs_read() {
                    body.push_str(&ent("com.apple.security.files.user-selected.read-only"));
                }
            }
            let entitlements = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <plist version=\"1.0\"><dict>\n{body}</dict></plist>\n"
            );
            vec![
                Entry { path: format!("{root}/Info.plist"), bytes: plist.into_bytes(), executable: false },
                Entry { path: format!("{root}/MacOS/{id}"), bytes: exe, executable: true },
                Entry { path: format!("{root}/Resources/{id}.entitlements"), bytes: entitlements.into_bytes(), executable: false },
                Entry { path: format!("{root}/Resources/permissions.json"), bytes: manifest, executable: false },
                Entry { path: format!("{root}/Resources/{id}.svg"), bytes: icon_svg(display).into_bytes(), executable: false },
            ]
        }
        "windows" => vec![
            Entry { path: format!("{display}/{id}.exe"), bytes: exe, executable: true },
            Entry { path: format!("{display}/permissions.json"), bytes: manifest, executable: false },
            Entry { path: format!("{display}/{id}.svg"), bytes: icon_svg(display).into_bytes(), executable: false },
        ],
        _ => {
            // An AppDir: what AppImage packs, and runnable as it is (`AppRun`). It also carries
            // a Flatpak manifest, whose sandbox is generated from the same inference.
            let root = format!("{display}.AppDir");
            let entry = |exec: &str, icon: &str| {
                format!(
                    "[Desktop Entry]\nType=Application\nName={display}\nExec={exec}\nIcon={icon}\nCategories=Utility;\nTerminal=false\n\
                     X-Purism-FormFactor=Workstation;Mobile;\nX-Helix-Permissions={summary}\n"
                )
            };
            let desktop = entry("AppRun", &id);
            let fid = flatpak_id(&id);
            vec![
                Entry { path: format!("{root}/{fid}.flatpak.json"), bytes: flatpak_manifest(&id, inf).into_bytes(), executable: false },
                Entry { path: format!("{root}/{fid}.desktop"), bytes: entry(&id, &fid).into_bytes(), executable: false },
                Entry { path: format!("{root}/AppRun"), bytes: exe, executable: true },
                Entry { path: format!("{root}/{id}.desktop"), bytes: desktop.into_bytes(), executable: false },
                Entry { path: format!("{root}/{id}.svg"), bytes: icon_svg(display).into_bytes(), executable: false },
                Entry { path: format!("{root}/.DirIcon"), bytes: icon_svg(display).into_bytes(), executable: false },
                Entry { path: format!("{root}/permissions.json"), bytes: manifest, executable: false },
            ]
        }
    }
}

/// The Flatpak application id for an app: reverse-DNS, as Flatpak requires.
pub fn flatpak_id(id: &str) -> String {
    format!("app.helix.{}", id.replace('-', "_"))
}

/// The Flatpak sandbox an app gets: what every windowed app needs to draw, plus exactly what
/// its code reaches (ADR 0064). Flatpak's file grants are by place, not by kind, so a file
/// reader gets the home folder read-only and a writer gets it read-write; finer places wait
/// on ADR 0021's per-path grants. Running programs needs no grant — programs inside the
/// sandbox run — and the host is never opened to it (`flatpak-spawn --host` would be an
/// escape, not a permission).
pub fn finish_args(inf: &Inferred) -> Vec<&'static str> {
    let mut a = vec!["--socket=wayland", "--socket=fallback-x11", "--share=ipc", "--device=dri"];
    if inf.caps.net_on() {
        a.push("--share=network");
    }
    if inf.caps.fs_write() {
        a.push("--filesystem=home");
    } else if inf.caps.fs_read() {
        a.push("--filesystem=home:ro");
    }
    a
}

/// A `flatpak-builder` manifest that packs this AppDir, with the sandbox from `finish_args`.
/// The GNOME runtime carries WebKitGTK, which the window uses.
pub fn flatpak_manifest(id: &str, inf: &Inferred) -> String {
    let fid = flatpak_id(id);
    let doc = serde_json::json!({
        "app-id": fid,
        "runtime": "org.gnome.Platform",
        "runtime-version": "48",
        "sdk": "org.gnome.Sdk",
        "command": id,
        "finish-args": finish_args(inf),
        "modules": [{
            "name": id,
            "buildsystem": "simple",
            "build-commands": [
                format!("install -Dm755 AppRun /app/bin/{id}"),
                format!("install -Dm644 {fid}.desktop /app/share/applications/{fid}.desktop"),
                format!("install -Dm644 {id}.svg /app/share/icons/hicolor/scalable/apps/{fid}.svg"),
                "install -Dm644 permissions.json /app/share/helix/permissions.json".to_string(),
            ],
            "sources": [{"type": "dir", "path": "."}],
        }],
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// The host's bundle kind.
pub fn host_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infer_src(src: &str) -> Inferred {
        let toks = crate::lexer::lex(src).unwrap();
        let stmts = crate::parser::parse(toks).unwrap();
        let spans = vec![Span { start_line: 1, source: src.into(), filename: "app.helix".into(), key: "app.helix".into() }];
        infer(&stmts, &spans)
    }

    #[test]
    fn permissions_are_what_the_code_reaches() {
        let none = infer_src("fn update(m, e) = m + 1\nfn view(m) = \"{m}\"\nprint(update(1, 0))\n");
        assert_eq!(none.caps.fs, None);
        assert!(!none.caps.net_on() && !none.caps.process_on());
        assert!(none.grants.is_empty());

        let reads = infer_src("fn load(p) = read_text(p)\nfn update(m, e) = {...m, text: load(e.value)}\n");
        assert_eq!(reads.caps.fs.as_deref(), Some("read"));
        let why = &reads.grants.iter().find(|(e, _)| *e == Effect::FsRead).unwrap().1;
        assert!(why.join(" -> ").ends_with("load -> read_text"), "{why:?}");

        // Top-level code counts too.
        let top = infer_src("DATA = http_get(\"https://example.com\")\n");
        assert!(top.caps.net_on());
    }

    #[test]
    fn bundles_have_the_platforms_shape() {
        let inf = infer_src("fn save(t) = mkdir(t)\n");
        let paths = |os| layout(os, "Habit Tracker", b"exe".to_vec(), &inf, "1.0.0").into_iter().map(|e| e.path).collect::<Vec<_>>();
        let linux = paths("linux");
        assert!(linux.contains(&"Habit Tracker.AppDir/AppRun".to_string()), "{linux:?}");
        assert!(linux.contains(&"Habit Tracker.AppDir/habit-tracker.desktop".to_string()));
        let mac = layout("macos", "Habit Tracker", b"exe".to_vec(), &inf, "1.0.0");
        let ent = mac.iter().find(|e| e.path.ends_with(".entitlements")).unwrap();
        let ent = String::from_utf8_lossy(&ent.bytes);
        assert!(ent.contains("app-sandbox") && ent.contains("read-write") && !ent.contains("network.client"), "{ent}");
        assert!(mac.iter().any(|e| e.path == "Habit Tracker.app/Contents/MacOS/habit-tracker" && e.executable));
        assert!(paths("windows").contains(&"Habit Tracker/habit-tracker.exe".to_string()));
        // The Flatpak sandbox is the inference: a writer gets home read-write, no network.
        assert!(linux.contains(&"Habit Tracker.AppDir/app.helix.habit_tracker.flatpak.json".to_string()), "{linux:?}");
        assert_eq!(finish_args(&inf), ["--socket=wayland", "--socket=fallback-x11", "--share=ipc", "--device=dri", "--filesystem=home"]);
        let none = infer_src("fn update(m, e) = m\n");
        assert!(!finish_args(&none).iter().any(|a| a.starts_with("--share=network") || a.starts_with("--filesystem")));
        let m: serde_json::Value = serde_json::from_str(&flatpak_manifest("habit-tracker", &inf)).unwrap();
        assert_eq!(m["app-id"], "app.helix.habit_tracker");
        assert_eq!(m["command"], "habit-tracker");
        assert_eq!(display_name("habit_tracker"), "Habit Tracker");
        assert_eq!(slug("Ünïcode App!"), "n-code-app");
        assert_eq!(base_name("m0$serve_tcp$265"), "serve_tcp");
        assert_eq!(base_name("m12$main"), "main");
        assert_eq!(base_name("bytes_of$254"), "bytes_of");
        assert_eq!(base_name("my$name"), "my$name");
    }
}
