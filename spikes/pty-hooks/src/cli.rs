//! Hand-written argument parsing for `chs` (§4.2).

use std::path::PathBuf;

use crate::paste::{InputEncoding, Lossy, Newline, NewlineKey};

#[derive(Debug, Clone)]
pub struct Args {
    pub positional: Vec<String>,
    pub root: PathBuf,
    pub out: Option<PathBuf>,
    /// Run id a parent assigns to a child run (checked against its report).
    pub run_id: Option<String>,
    pub deadline_secs: u64,
    pub session_start_timeout: u64,
    pub keep_temp: bool,
    pub cols: u16,
    pub rows: u16,
    // real
    pub prompt: bool,
    pub claude_bin: Option<PathBuf>,
    pub codex_bin: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub allow_api_usage: bool,
    pub agent: Option<String>,
    pub attended: bool,
    pub subset: Option<String>,
    // paste
    pub paste_newline: Option<Newline>,
    pub enter_delay_ms: u64,
    pub chunk: usize,
    // codex
    pub launches: u32,
    pub mutate_command: bool,
    pub cwd2: Option<PathBuf>,
    pub codex_mode: String,
    // fake / probes
    pub hold_assign: bool,
    pub escape_probe: bool,
    pub detached: bool,
    pub confirm_close: bool,
    pub url: Option<String>,
    pub case: Option<String>,
    pub include_pty_output: bool,
    pub explicit_deadline: bool,
    // input (F-1)
    /// `auto` (adapter capability), `bracketed`, or `win-keys`.
    pub input_encoding: String,
    pub newline_key: NewlineKey,
    /// Set by `--newline-key` (forwarded to children only then).
    pub explicit_newline_key: bool,
    pub allow_lossy: Vec<Lossy>,
    // pty (F-2)
    /// `portable` or `raw` (Windows only).
    pub pty_backend: String,
    pub conpty_flags: u32,
    pub raw_inject: Option<String>,
    // C-5 / F-2 verdict
    pub configs: Vec<String>,
    pub join_run_id: Option<String>,
    pub from: Vec<PathBuf>,
    /// F-1's winning newline key for `win-probe backend` (`none`: no winner).
    pub f1_winner: Option<NewlineKey>,
}

/// F-2c fault injections for the raw backend.
pub const RAW_INJECTS: &[&str] = &[
    "before-spawn",
    "after-spawn",
    "reader-start",
    "force-suspended",
    "resume-fail",
];

/// F-2 configurations: `P` (portable-pty), `R0` / `R6` (raw, flags 0 / 0x6).
pub const CONFIGS: &[&str] = &["P", "R0", "R6"];

/// Backend flags of one F-2 configuration.
pub fn config_flags(config: &str) -> Option<Vec<String>> {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect();
    match config {
        "P" => Some(v(&["--pty-backend", "portable"])),
        "R0" => Some(v(&["--pty-backend", "raw", "--conpty-flags", "0"])),
        "R6" => Some(v(&["--pty-backend", "raw", "--conpty-flags", "6"])),
        _ => None,
    }
}

impl Args {
    /// The input encoding for `agent`: the adapter capability unless
    /// `--input-encoding` overrides it for an experiment.
    pub fn encoding_for(&self, agent: &str) -> InputEncoding {
        match self.input_encoding.as_str() {
            "bracketed" => InputEncoding::BracketedPaste,
            "win-keys" => InputEncoding::WinKeys {
                newline: self.newline_key,
            },
            _ => crate::paste::input_encoding(agent, crate::os::os_name(), self.newline_key),
        }
    }
}

pub const USAGE: &str = "\
usage:
  chs install-emitter [--root R]
  chs fake <paste|queries|stall|tree-kill|auth|emit|teardown|all> [--hold-assign-until-child]
  chs fake tree-kill --escape-probe          (macOS containment limit; not part of `all`)
  chs fake raw-inject --pty-backend raw --raw-inject X   (Windows F-2c; not part of `all`)
  chs real <claude-hooks|claude-paste|claude-stall|codex-hooks|codex-paste|codex-stall|codex-warm|tree-kill>
           --allow-api-usage [--agent claude|codex]
  chs pretrust <claude|codex>               (launches the CLI in each folder to trust)
  chs print-launch <claude|codex>           (prints the launch argv; diagnostics)
  chs win-probe <paste|startup|kill|attach|console|all> [--detached]
  chs win-probe attach-client --url U [--confirm-close]   (credential on stdin)
  chs bundle [--include-pty-output]
common: --root R  --out DIR  --deadline-secs 180  --session-start-timeout 20  --keep-temp  --cols 120 --rows 40
real:   --prompt  --claude-bin PATH  --codex-bin PATH  --cwd DIR  --attended  --subset <required|f1|f1-recorded>
paste:  --paste-newline <cr|lf|crlf>  --enter-delay-ms 300  --chunk N
codex:  --hook-source session-flags  --launches N
        --mutate-command  --cwd2 DIR  --codex-mode <no-daemon|default|both>
input:  --input-encoding <auto|bracketed|win-keys>  --newline-key <csi-u|w32>  --allow-lossy <trim|tab>
pty:    --pty-backend <portable|raw>  --conpty-flags <0|6>  --raw-inject <before-spawn|after-spawn|reader-start|force-suspended|resume-fail>
f-run:  chs win-probe <codex-input|backend|followup>  (backend: --f1-winner <csi-u|w32|none>)
        chs win-probe rerun   (phase 3b: F-1 w32, tree kills + codex_input per config, F-2c)
        chs win-probe console --detached --configs P,R0,R6 --join-run-id ID
        chs win-probe f2-verdict --from DIR --from DIR";

/// Default `--session-start-timeout` in seconds.
pub const SESSION_START_DEFAULT: u64 = 20;

/// The only hook source built in this spike (§4.3).
pub const HOOK_SOURCE: &str = "session-flags";

pub const HOOK_SOURCE_NOT_BUILT: &str =
    "codex-home mode is not built; requires A-3 result showing session-flags cannot persist trust";

fn value(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn num<T: std::str::FromStr>(s: &str, flag: &str) -> Result<T, String> {
    s.parse()
        .map_err(|_| format!("{flag}: invalid number {s:?}"))
}

pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        positional: Vec::new(),
        root: crate::os::default_root(),
        out: None,
        run_id: None,
        deadline_secs: 180,
        session_start_timeout: SESSION_START_DEFAULT,
        keep_temp: false,
        cols: 120,
        rows: 40,
        prompt: false,
        claude_bin: None,
        codex_bin: None,
        cwd: None,
        allow_api_usage: false,
        agent: None,
        attended: false,
        subset: None,
        paste_newline: None,
        enter_delay_ms: 300,
        chunk: 0,
        launches: 1,
        mutate_command: false,
        cwd2: None,
        codex_mode: "no-daemon".into(),
        hold_assign: false,
        escape_probe: false,
        detached: false,
        confirm_close: false,
        url: None,
        case: None,
        include_pty_output: false,
        explicit_deadline: false,
        input_encoding: "auto".into(),
        newline_key: NewlineKey::CsiU,
        explicit_newline_key: false,
        allow_lossy: Vec::new(),
        pty_backend: "portable".into(),
        conpty_flags: 0,
        raw_inject: None,
        configs: Vec::new(),
        join_run_id: None,
        from: Vec::new(),
        f1_winner: None,
    };
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let f = arg.as_str();
        match f {
            "--root" => a.root = PathBuf::from(value(&mut it, f)?),
            "--out" => a.out = Some(PathBuf::from(value(&mut it, f)?)),
            "--run-id" => {
                let v = value(&mut it, f)?;
                if v.is_empty() || v.len() > 32 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("--run-id {v:?}: expected hex"));
                }
                a.run_id = Some(v);
            }
            "--deadline-secs" => {
                a.deadline_secs = num(&value(&mut it, f)?, f)?;
                a.explicit_deadline = true;
            }
            "--session-start-timeout" => a.session_start_timeout = num(&value(&mut it, f)?, f)?,
            "--keep-temp" => a.keep_temp = true,
            "--cols" => a.cols = num(&value(&mut it, f)?, f)?,
            "--rows" => a.rows = num(&value(&mut it, f)?, f)?,
            "--prompt" => a.prompt = true,
            "--claude-bin" => a.claude_bin = Some(PathBuf::from(value(&mut it, f)?)),
            "--codex-bin" => a.codex_bin = Some(PathBuf::from(value(&mut it, f)?)),
            "--cwd" => a.cwd = Some(PathBuf::from(value(&mut it, f)?)),
            "--allow-api-usage" => a.allow_api_usage = true,
            "--agent" => a.agent = Some(value(&mut it, f)?),
            "--attended" => a.attended = true,
            "--subset" => a.subset = Some(value(&mut it, f)?),
            "--paste-newline" => {
                let v = value(&mut it, f)?;
                a.paste_newline =
                    Some(Newline::parse(&v).ok_or_else(|| format!("{f}: expected cr|lf|crlf"))?);
            }
            "--enter-delay-ms" => a.enter_delay_ms = num(&value(&mut it, f)?, f)?,
            "--chunk" => a.chunk = num(&value(&mut it, f)?, f)?,
            "--hook-source" => {
                if value(&mut it, f)? != HOOK_SOURCE {
                    return Err(HOOK_SOURCE_NOT_BUILT.into());
                }
            }
            "--launches" => a.launches = num(&value(&mut it, f)?, f)?,
            "--mutate-command" => a.mutate_command = true,
            "--cwd2" => a.cwd2 = Some(PathBuf::from(value(&mut it, f)?)),
            "--codex-mode" => {
                a.codex_mode = value(&mut it, f)?;
                if !["no-daemon", "default", "both"].contains(&a.codex_mode.as_str()) {
                    return Err(format!("{f}: expected no-daemon|default|both"));
                }
            }
            "--hold-assign-until-child" => a.hold_assign = true,
            "--escape-probe" => a.escape_probe = true,
            "--detached" => a.detached = true,
            "--confirm-close" => a.confirm_close = true,
            "--url" => a.url = Some(value(&mut it, f)?),
            "--case" => a.case = Some(value(&mut it, f)?),
            "--include-pty-output" => a.include_pty_output = true,
            "--input-encoding" => {
                a.input_encoding = value(&mut it, f)?;
                if !["auto", "bracketed", "win-keys"].contains(&a.input_encoding.as_str()) {
                    return Err(format!("{f}: expected auto|bracketed|win-keys"));
                }
            }
            "--newline-key" => {
                let v = value(&mut it, f)?;
                a.newline_key =
                    NewlineKey::parse(&v).ok_or_else(|| format!("{f}: expected csi-u|w32"))?;
                a.explicit_newline_key = true;
            }
            "--allow-lossy" => {
                let v = value(&mut it, f)?;
                let l = Lossy::parse(&v).ok_or_else(|| format!("{f}: expected trim|tab"))?;
                if !a.allow_lossy.contains(&l) {
                    a.allow_lossy.push(l);
                }
            }
            "--pty-backend" => {
                a.pty_backend = value(&mut it, f)?;
                if !["portable", "raw"].contains(&a.pty_backend.as_str()) {
                    return Err(format!("{f}: expected portable|raw"));
                }
            }
            "--conpty-flags" => {
                a.conpty_flags = match value(&mut it, f)?.as_str() {
                    "0" => 0,
                    "6" => 6,
                    _ => return Err(format!("{f}: expected 0|6")),
                };
            }
            "--raw-inject" => {
                let v = value(&mut it, f)?;
                if !RAW_INJECTS.contains(&v.as_str()) {
                    return Err(format!("{f}: expected one of {RAW_INJECTS:?}"));
                }
                a.raw_inject = Some(v);
            }
            "--configs" => {
                let v = value(&mut it, f)?;
                a.configs = v.split(',').map(str::to_string).collect();
                if a.configs.is_empty() || a.configs.iter().any(|c| !CONFIGS.contains(&c.as_str()))
                {
                    return Err(format!("{f}: expected a comma list of {CONFIGS:?}"));
                }
            }
            "--join-run-id" => {
                let v = value(&mut it, f)?;
                if v.is_empty() || v.len() > 32 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("{f} {v:?}: expected hex"));
                }
                a.join_run_id = Some(v);
            }
            "--from" => a.from.push(PathBuf::from(value(&mut it, f)?)),
            "--f1-winner" => {
                let v = value(&mut it, f)?;
                a.f1_winner = match v.as_str() {
                    "none" => None,
                    k => Some(
                        NewlineKey::parse(k)
                            .ok_or_else(|| format!("{f}: expected csi-u|w32|none"))?,
                    ),
                };
            }
            "-h" | "--help" => return Err(USAGE.into()),
            _ if f.starts_with('-') => return Err(format!("unknown flag {f}\n{USAGE}")),
            _ => a.positional.push(arg.clone()),
        }
    }
    // The raw backend is Windows-only, and its tree is the job from creation.
    if a.pty_backend == "raw" && crate::os::os_name() != "windows" {
        return Err("--pty-backend raw is Windows only".into());
    }
    if a.pty_backend != "raw" && (a.raw_inject.is_some() || a.conpty_flags != 0) {
        return Err("--conpty-flags and --raw-inject need --pty-backend raw".into());
    }
    if a.pty_backend == "raw" && a.hold_assign {
        return Err("--hold-assign-until-child does not apply to --pty-backend raw".into());
    }
    if a.deadline_secs <= 10 {
        return Err("--deadline-secs must exceed 10 (scenario budget = deadline - 10 s)".into());
    }
    Ok(a)
}

/// Flags to forward when `chs` runs itself as a child scenario.
pub fn forward_common(a: &Args) -> Vec<String> {
    let mut v = vec![
        "--root".to_string(),
        a.root.display().to_string(),
        "--session-start-timeout".into(),
        a.session_start_timeout.to_string(),
        "--cols".into(),
        a.cols.to_string(),
        "--rows".into(),
        a.rows.to_string(),
        "--enter-delay-ms".into(),
        a.enter_delay_ms.to_string(),
    ];
    if a.keep_temp {
        v.push("--keep-temp".into());
    }
    if a.allow_api_usage {
        v.push("--allow-api-usage".into());
    }
    if let Some(p) = &a.claude_bin {
        v.extend(["--claude-bin".into(), p.display().to_string()]);
    }
    if let Some(p) = &a.codex_bin {
        v.extend(["--codex-bin".into(), p.display().to_string()]);
    }
    v.extend(forward_input(a));
    v.extend(forward_backend(a));
    v
}

/// The input options (forwarded like the common ones).
pub fn forward_input(a: &Args) -> Vec<String> {
    let mut v = Vec::new();
    if a.input_encoding != "auto" {
        v.extend(["--input-encoding".into(), a.input_encoding.clone()]);
    }
    if a.explicit_newline_key {
        v.extend(["--newline-key".into(), a.newline_key.name().into()]);
    }
    for l in &a.allow_lossy {
        v.extend(["--allow-lossy".into(), l.name().into()]);
    }
    v
}

/// The backend options (forwarded like the common ones).
pub fn forward_backend(a: &Args) -> Vec<String> {
    let mut v = Vec::new();
    if a.pty_backend != "portable" {
        v.extend(["--pty-backend".into(), a.pty_backend.clone()]);
        v.extend(["--conpty-flags".into(), a.conpty_flags.to_string()]);
    }
    if let Some(i) = &a.raw_inject {
        v.extend(["--raw-inject".into(), i.clone()]);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    /// Unix paths stay as before: every new option defaults to today's path,
    /// and the raw backend is refused off Windows.
    #[test]
    fn new_options_default_to_todays_path() {
        let a = p(&["fake", "paste"]).unwrap();
        assert_eq!(a.pty_backend, "portable");
        assert_eq!(a.encoding_for("claude"), InputEncoding::BracketedPaste);
        assert!(forward_input(&a).is_empty() && forward_backend(&a).is_empty());
        if crate::os::os_name() != "windows" {
            assert!(p(&["fake", "paste", "--pty-backend", "raw"]).is_err());
            assert_eq!(a.encoding_for("codex"), InputEncoding::BracketedPaste);
        }
        assert!(p(&["fake", "paste", "--raw-inject", "before-spawn"]).is_err());
        let a = p(&[
            "real",
            "codex-paste",
            "--input-encoding",
            "win-keys",
            "--newline-key",
            "w32",
            "--allow-lossy",
            "tab",
        ])
        .unwrap();
        assert_eq!(
            a.encoding_for("codex"),
            InputEncoding::WinKeys {
                newline: NewlineKey::W32
            }
        );
        assert_eq!(
            forward_input(&a),
            [
                "--input-encoding",
                "win-keys",
                "--newline-key",
                "w32",
                "--allow-lossy",
                "tab"
            ]
        );
        assert!(p(&["x", "--configs", "P,R7"]).is_err());
        assert_eq!(p(&["x", "--configs", "P,R0,R6"]).unwrap().configs, CONFIGS);
    }
}
