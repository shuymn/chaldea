//! `chaldea-fake-cli` (`docs/design/pty-unix.md` §2.8):
//!
//! ```text
//! chaldea-fake-cli --mode <echo|queries|attachable|spawn-tree|sleeper|report-process|hooks|banner>
//!                  [--report <127.0.0.1:port>] [--role <role>] [--roles <list>]
//!                  [--max-life-secs <n>] [--reply-timeout-ms <n>] [--raw] [--setsid]
//!                  [--escape-orphan] [--orphan-keeps-tty] [--out <path>] [--emit-output]
//!                  [--validate-input <n>] [--emitter <path>] [--agent <claude|codex>]
//!                  [--session-id <id>] [--nested] [--nested-child] [--script <json>]
//!                  [-- <ignored argument>...]
//! ```
//!
//! `--emitter`, `--agent`, `--session-id`, `--nested`, `--nested-child`, and `--script` are
//! the hook emission of `docs/design/hook-ingest.md` §2.14 (`echo` and `hooks`).
//!
//! An unknown mode or flag exits 2. Arguments after `--` are ignored (`report-process`
//! records them with the rest of its argv; `docs/design/pty-windows.md` §2.8).

mod attachable;
mod banner;
mod echo;
mod hooks;
mod queries;
mod report_process;
mod sleeper;
mod spawn_tree;
mod term;
mod watch;

use std::ffi::OsString;
use std::time::Duration;

use serde_json::{Value, json};

use crate::report::Reporter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Echo,
    Queries,
    Attachable,
    SpawnTree,
    Sleeper,
    ReportProcess,
    Hooks,
    Banner,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Echo => "echo",
            Mode::Queries => "queries",
            Mode::Attachable => "attachable",
            Mode::SpawnTree => "spawn-tree",
            Mode::Sleeper => "sleeper",
            Mode::ReportProcess => "report-process",
            Mode::Hooks => "hooks",
            Mode::Banner => "banner",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Opts {
    pub(crate) mode: Mode,
    pub(crate) report: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) roles: Option<Vec<String>>,
    pub(crate) max_life_secs: Option<u64>,
    pub(crate) reply_timeout: Duration,
    pub(crate) raw: bool,
    pub(crate) setsid: bool,
    pub(crate) escape_orphan: bool,
    pub(crate) orphan_keeps_tty: bool,
    /// `report-process`: the file to write.
    pub(crate) out: Option<String>,
    /// `spawn-tree`: write `OUTPUT-<n>` lines and acknowledge each.
    pub(crate) emit_output: bool,
    /// `echo`: read exactly this many bytes and check the `a`–`z` pattern.
    pub(crate) validate_input: Option<usize>,
    /// `echo`, `hooks`: the emitter binary, run as `<emitter> hook-emit <agent>`.
    pub(crate) emitter: Option<String>,
    /// The emitter's agent argument (`codex` by default).
    pub(crate) agent: String,
    /// `echo --emitter`: the session id of its hooks (a random UUID by default).
    pub(crate) session_id: Option<String>,
    /// `echo --emitter`: also start a nested FakeCli that emits another session's hooks.
    pub(crate) nested: bool,
    /// `echo`: run as that nested FakeCli.
    pub(crate) nested_child: bool,
    /// `hooks`: the steps, a JSON array.
    pub(crate) script: Option<String>,
}

fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Opts, String> {
    let mut it = args
        .into_iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned());
    let mut mode = None;
    let mut o = Opts {
        mode: Mode::Sleeper,
        report: None,
        role: None,
        roles: None,
        max_life_secs: None,
        reply_timeout: Duration::from_millis(500),
        raw: false,
        setsid: false,
        escape_orphan: false,
        orphan_keeps_tty: false,
        out: None,
        emit_output: false,
        validate_input: None,
        emitter: None,
        agent: "codex".to_owned(),
        session_id: None,
        nested: false,
        nested_child: false,
        script: None,
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--mode" => {
                mode = Some(match value()?.as_str() {
                    "echo" => Mode::Echo,
                    "queries" => Mode::Queries,
                    "attachable" => Mode::Attachable,
                    "spawn-tree" => Mode::SpawnTree,
                    "sleeper" => Mode::Sleeper,
                    "report-process" => Mode::ReportProcess,
                    "hooks" => Mode::Hooks,
                    "banner" => Mode::Banner,
                    m => return Err(format!("unknown mode {m}")),
                });
            }
            "--report" => o.report = Some(value()?),
            "--role" => o.role = Some(value()?),
            "--roles" => {
                let v = value()?;
                o.roles = Some(if v == "none" {
                    Vec::new()
                } else {
                    v.split(',').map(str::to_owned).collect()
                });
            }
            "--max-life-secs" => {
                o.max_life_secs = Some(value()?.parse().map_err(|e| format!("{flag}: {e}"))?);
            }
            "--reply-timeout-ms" => {
                let ms: u64 = value()?.parse().map_err(|e| format!("{flag}: {e}"))?;
                o.reply_timeout = Duration::from_millis(ms);
            }
            "--raw" => o.raw = true,
            "--setsid" => o.setsid = true,
            "--escape-orphan" => o.escape_orphan = true,
            "--orphan-keeps-tty" => o.orphan_keeps_tty = true,
            "--out" => o.out = Some(value()?),
            "--emit-output" => o.emit_output = true,
            "--validate-input" => {
                o.validate_input = Some(value()?.parse().map_err(|e| format!("{flag}: {e}"))?);
            }
            "--emitter" => o.emitter = Some(value()?),
            "--agent" => o.agent = value()?,
            "--session-id" => o.session_id = Some(value()?),
            "--nested" => o.nested = true,
            "--nested-child" => o.nested_child = true,
            "--script" => o.script = Some(value()?),
            "--" => break,
            f => return Err(format!("unknown flag {f}")),
        }
    }
    o.mode = mode.ok_or("--mode is required")?;
    Ok(o)
}

/// Runs FakeCli with `args` (`args[0]` is the program name); returns the exit code.
pub fn main(args: impl IntoIterator<Item = OsString>) -> i32 {
    let o = match parse(args) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("chaldea-fake-cli: {msg}");
            return 2;
        }
    };
    if let Some(secs) = o.max_life_secs {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            crate::report::end();
            std::process::exit(0);
        });
    }
    let code = match o.mode {
        Mode::Echo => echo::run(&o),
        Mode::Queries => queries::run(&o),
        Mode::Attachable => attachable::run(&o),
        Mode::SpawnTree => spawn_tree::run(&o),
        Mode::Sleeper => sleeper::run(&o),
        Mode::ReportProcess => report_process::run(&o),
        Mode::Hooks => hooks::run(&o),
        Mode::Banner => banner::run(&o),
    };
    crate::report::end();
    code
}

/// Lists the fds, connects the report stream, and sends the hello. The fds are listed
/// before connecting, so the stream is not among them.
pub(crate) fn hello(o: &Opts, raw: bool) -> Reporter {
    let fds = term::open_fds();
    let rep = Reporter::connect(o.report.as_deref(), term::inq);
    let role = o.role.clone().unwrap_or_else(|| o.mode.name().to_owned());
    let mut v = json!({
        "event": "hello",
        "mode": o.mode.name(),
        "role": role,
        "pid": std::process::id(),
        "raw": raw,
        "fds": fds,
    });
    if let Value::Object(m) = &mut v
        && let Value::Object(t) = term::identity()
    {
        m.extend(t);
    }
    rep.send(&v);
    rep
}

/// Enters raw mode when asked (`--raw`) or when the mode reads stdin, only on a terminal.
pub(crate) fn enter_raw(o: &Opts, reads_stdin: bool) -> bool {
    (o.raw || reads_stdin) && term::stdin_is_tty() && term::make_raw()
}

/// The arguments of a `sleeper` child FakeCli process with `role`, and what it inherits.
pub(crate) fn child_args(o: &Opts, role: &str) -> Vec<String> {
    let mut a = vec![
        "--mode".to_owned(),
        "sleeper".to_owned(),
        "--role".to_owned(),
        role.to_owned(),
    ];
    a.extend(inherited_args(o));
    a
}

/// The arguments every child FakeCli process inherits: report address and life bound.
pub(crate) fn inherited_args(o: &Opts) -> Vec<String> {
    let mut a = Vec::new();
    if let Some(r) = &o.report {
        a.extend(["--report".to_owned(), r.clone()]);
    }
    if let Some(s) = o.max_life_secs {
        a.extend(["--max-life-secs".to_owned(), s.to_string()]);
    }
    a
}

/// Sleeps until the process is ended by a signal or `--max-life-secs`.
pub(crate) fn idle() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_inherits_the_report_and_life_bound() {
        let args = |a: &[&str]| {
            let mut v = vec![OsString::from("chaldea-fake-cli")];
            v.extend(a.iter().map(OsString::from));
            parse(v).unwrap()
        };
        let o = args(&[
            "--mode",
            "hooks",
            "--report",
            "127.0.0.1:9",
            "--max-life-secs",
            "60",
        ]);
        assert_eq!(
            inherited_args(&o),
            ["--report", "127.0.0.1:9", "--max-life-secs", "60"]
        );
        assert!(inherited_args(&args(&["--mode", "hooks"])).is_empty());
        assert_eq!(
            child_args(&o, "r")[..4],
            ["--mode", "sleeper", "--role", "r"]
        );
    }
}
