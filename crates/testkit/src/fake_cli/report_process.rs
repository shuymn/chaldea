//! `report-process --out <path>`: writes what the process was started with to `<path>.tmp`,
//! renames it to `<path>`, and exits 0. It opens no report stream, so it runs with an
//! empty environment (no `SystemRoot`, no Winsock).

use std::io::IsTerminal;

use serde_json::json;

use super::Opts;

pub(crate) fn run(o: &Opts) -> i32 {
    let Some(out) = &o.out else {
        eprintln!("chaldea-fake-cli: report-process needs --out");
        return 2;
    };
    let lossy = |s: std::ffi::OsString| s.to_string_lossy().into_owned();
    let argv: Vec<String> = std::env::args_os().map(lossy).collect();
    let env: Vec<[String; 2]> = std::env::vars_os()
        .map(|(k, v)| [lossy(k), lossy(v)])
        .collect();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| lossy(p.into_os_string()));
    let v = json!({
        "argv": argv,
        "env": env,
        "cwd": cwd,
        "stdin_console": std::io::stdin().is_terminal(),
        "stdout_console": std::io::stdout().is_terminal(),
        "stderr_console": std::io::stderr().is_terminal(),
    });
    let tmp = format!("{out}.tmp");
    if std::fs::write(&tmp, v.to_string()).is_err() || std::fs::rename(&tmp, out).is_err() {
        return 1;
    }
    0
}
