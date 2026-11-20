//! Hand-written argument parsing for `chs` (§4.2).

use std::path::PathBuf;

use crate::paste::Newline;

#[derive(Debug, Clone)]
pub struct Args {
    pub positional: Vec<String>,
    pub root: PathBuf,
    pub out: Option<PathBuf>,
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
    pub hook_source: String,
    pub i_approve_global_hooks: bool,
    pub launches: u32,
    pub mutate_command: bool,
    pub cwd2: Option<PathBuf>,
    pub codex_mode: String,
    // fake / probes
    pub hold_assign: bool,
    pub detached: bool,
    pub url: Option<String>,
    pub case: Option<String>,
    pub include_pty_output: bool,
    pub explicit_deadline: bool,
}

pub const USAGE: &str = "\
usage:
  chs install-emitter [--root R]
  chs fake <paste|queries|stall|tree-kill|auth|emit|teardown|all> [--hold-assign-until-child]
  chs real <claude-hooks|claude-paste|claude-stall|codex-hooks|codex-paste|codex-stall|codex-warm|tree-kill>
           --allow-api-usage [--agent claude|codex]
  chs print-launch <claude|codex>
  chs win-probe <paste|startup|kill|attach|console|all> [--detached]
  chs win-probe attach-client --url U        (credential on stdin)
  chs bundle [--include-pty-output]
common: --root R  --out DIR  --deadline-secs 180  --session-start-timeout 20  --keep-temp  --cols 120 --rows 40
real:   --prompt  --claude-bin PATH  --codex-bin PATH  --cwd DIR  --attended  --subset required
paste:  --paste-newline <cr|lf|crlf>  --enter-delay-ms 300  --chunk N
codex:  --hook-source <session-flags|codex-home|project> [--i-approve-global-hooks]  --launches N
        --mutate-command  --cwd2 DIR  --codex-mode <no-daemon|default|both>";

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
        deadline_secs: 180,
        session_start_timeout: 20,
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
        hook_source: "session-flags".into(),
        i_approve_global_hooks: false,
        launches: 1,
        mutate_command: false,
        cwd2: None,
        codex_mode: "no-daemon".into(),
        hold_assign: false,
        detached: false,
        url: None,
        case: None,
        include_pty_output: false,
        explicit_deadline: false,
    };
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let f = arg.as_str();
        match f {
            "--root" => a.root = PathBuf::from(value(&mut it, f)?),
            "--out" => a.out = Some(PathBuf::from(value(&mut it, f)?)),
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
                a.hook_source = value(&mut it, f)?;
                if !["session-flags", "codex-home", "project"].contains(&a.hook_source.as_str()) {
                    return Err(format!("{f}: expected session-flags|codex-home|project"));
                }
            }
            "--i-approve-global-hooks" => a.i_approve_global_hooks = true,
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
            "--detached" => a.detached = true,
            "--url" => a.url = Some(value(&mut it, f)?),
            "--case" => a.case = Some(value(&mut it, f)?),
            "--include-pty-output" => a.include_pty_output = true,
            "-h" | "--help" => return Err(USAGE.into()),
            _ if f.starts_with('-') => return Err(format!("unknown flag {f}\n{USAGE}")),
            _ => a.positional.push(arg.clone()),
        }
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
    v
}
