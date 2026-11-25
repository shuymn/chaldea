//! The launch plan (`docs/design/supervisor.md` §2.7): the arguments after the program, the
//! child's complete environment, and the facts the reservation records, from `(agent,
//! host_os)` and the launch's data. Pure: the host's environment-key comparison comes in as
//! two functions (the daemon passes `chaldea_pty_runtime::{env_key_eq, env_key_has_prefix}`).

use std::ffi::{OsStr, OsString};
use std::fmt;

use sha2::{Digest, Sha256};

use crate::adapter::HostOs;
use crate::ids::{Agent, LaunchId, ThreadId, WorkspaceId, WorktreeId};

/// One entry of [`ENV_SCRUB`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scrub {
    /// The variable of this name.
    Name(&'static str),
    /// Every variable whose name starts with this prefix.
    Prefix(&'static str),
}

/// The identity variables removed from the daemon's environment before it is handed to a
/// launched CLI (`docs/mvp-design.md` §2.7 "Env scrub"): by name, so the user's own
/// configuration under the same prefixes (`CODEX_HOME`, `CLAUDE_CODE_USE_BEDROCK`) stays,
/// plus every `CHALDEA_*` variable. Names are compared as the host compares environment keys.
pub const ENV_SCRUB: [Scrub; 7] = [
    Scrub::Name("CLAUDECODE"),
    Scrub::Name("CLAUDE_CODE_CHILD_SESSION"),
    Scrub::Name("CLAUDE_CODE_SESSION_ID"),
    Scrub::Name("CLAUDE_CODE_MESSAGING_TOKEN"),
    Scrub::Name("CODEX_THREAD_ID"),
    Scrub::Name("CODEX_SESSION_ID"),
    Scrub::Prefix("CHALDEA_"),
];

/// The `TERM` a macOS launch gets: the VT is the daemon's, not the daemon's own terminal.
const MACOS_TERM: &str = "xterm-256color";

/// The Codex flag that turns off paste-burst detection on Windows, where prompts travel as
/// key records (`docs/design/input-encoders.md`).
const CODEX_WINDOWS_FLAGS: [&str; 2] = ["-c", "tui.disable_paste_burst=true"];

/// The host's environment-key equality and prefix test.
pub type KeyRule = fn(&OsStr, &OsStr) -> bool;

/// What a launch is planned from.
pub struct LaunchInput {
    pub agent: Agent,
    pub host_os: HostOs,
    pub thread_id: ThreadId,
    pub launch_id: LaunchId,
    /// The thread's bound session: set for a resume, which requires one.
    pub resume_session: Option<String>,
    /// A Codex positional first prompt; `prepare` refuses a Claude one, and `plan` never
    /// places one in a Claude argv.
    pub prompt: Option<String>,
    /// Set by PR 16's spawn path when the cwd is a managed worktree, and for a resume to the
    /// previous launch's.
    pub worktree_id: Option<WorktreeId>,
    /// The daemon's own environment.
    pub daemon_env: Vec<(OsString, OsString)>,
    /// The host's key equality (`env_key_eq`).
    pub key_eq: KeyRule,
    /// The host's prefix test (`env_key_has_prefix`).
    pub key_has_prefix: KeyRule,
    /// The daemon's published port.
    pub port: u16,
    /// This launch's hook token.
    pub hook_token: String,
}

impl fmt::Debug for LaunchInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchInput")
            .field("agent", &self.agent)
            .field("host_os", &self.host_os)
            .field("thread_id", &self.thread_id)
            .field("launch_id", &self.launch_id)
            .field("resume", &self.resume_session.is_some())
            .field("prompt", &self.prompt.as_ref().map(|_| "<redacted>"))
            .field("worktree_id", &self.worktree_id)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// A launch's argv after the program, its environment, and what the reservation records.
pub struct LaunchPlan {
    /// The arguments after the program.
    pub args: Vec<String>,
    /// The child's complete environment: nothing else is inherited.
    pub env: Vec<(OsString, OsString)>,
    /// Lowercase hex SHA-256 of `args`, each followed by a NUL byte (the reservation stores
    /// it instead of the argv, which can hold the prompt).
    pub argv_digest: String,
    /// Codex without a first prompt (`LaunchReserved.manual`).
    pub manual: bool,
    /// A Codex launch whose CLI version changed since its hooks were trusted: raises
    /// `startup_stall` at its start. False until PR 14 supplies the comparison.
    pub upgraded: bool,
    pub worktree_id: Option<WorktreeId>,
}

impl fmt::Debug for LaunchPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchPlan")
            .field("args", &self.args.len())
            .field("argv_digest", &self.argv_digest)
            .field("manual", &self.manual)
            .field("upgraded", &self.upgraded)
            .field("worktree_id", &self.worktree_id)
            .finish_non_exhaustive()
    }
}

/// The plan of one launch (`docs/design/supervisor.md` §2.7):
///
/// | Launch | Claude | Codex |
/// |---|---|---|
/// | Fresh | `--session-id <launch_id>` | `--no-daemon` [Windows flag] [`--` `<prompt>`] |
/// | Resume | `--resume <session_id>` | `resume` `--no-daemon` [Windows flag] `<session_id>` [`--` `<prompt>`] |
///
/// The prompt is one argument after a `--` that ends the argv. The environment is the
/// daemon's minus [`ENV_SCRUB`], plus `CHALDEA_URL`, `CHALDEA_HOOK_URL`, `CHALDEA_HOOK_TOKEN`,
/// `CHALDEA_THREAD_ID`, and on macOS `TERM=xterm-256color` in place of the daemon's `TERM`.
#[must_use]
pub fn plan(input: &LaunchInput) -> LaunchPlan {
    let args = args(input);
    LaunchPlan {
        argv_digest: argv_digest(&args),
        env: child_env(input),
        manual: input.agent == Agent::Codex && input.prompt.is_none(),
        upgraded: false,
        worktree_id: input.worktree_id.clone(),
        args,
    }
}

fn args(input: &LaunchInput) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    match input.agent {
        Agent::Claude => match &input.resume_session {
            Some(session) => a.extend(["--resume".to_owned(), session.clone()]),
            None => a.extend(["--session-id".to_owned(), input.launch_id.to_string()]),
        },
        Agent::Codex => {
            if input.resume_session.is_some() {
                a.push("resume".to_owned());
            }
            a.push("--no-daemon".to_owned());
            if input.host_os == HostOs::Windows {
                a.extend(CODEX_WINDOWS_FLAGS.map(str::to_owned));
            }
            if let Some(session) = &input.resume_session {
                a.push(session.clone());
            }
            if let Some(prompt) = &input.prompt {
                a.extend(["--".to_owned(), prompt.clone()]);
            }
        }
    }
    a
}

fn argv_digest(args: &[String]) -> String {
    let mut h = Sha256::new();
    for a in args {
        h.update(a.as_bytes());
        h.update([0]);
    }
    hex(&h.finalize())
}

/// Whether `key` is removed by [`ENV_SCRUB`] under the host's comparison.
fn scrubbed(key: &OsStr, eq: KeyRule, has_prefix: KeyRule) -> bool {
    ENV_SCRUB.iter().any(|s| match s {
        Scrub::Name(n) => eq(key, OsStr::new(n)),
        Scrub::Prefix(p) => has_prefix(key, OsStr::new(p)),
    })
}

fn child_env(input: &LaunchInput) -> Vec<(OsString, OsString)> {
    let macos = input.host_os == HostOs::Macos;
    let term = OsStr::new("TERM");
    let mut env: Vec<(OsString, OsString)> = input
        .daemon_env
        .iter()
        .filter(|(k, _)| !scrubbed(k, input.key_eq, input.key_has_prefix))
        .filter(|(k, _)| !(macos && (input.key_eq)(k, term)))
        .cloned()
        .collect();
    let url = format!("http://127.0.0.1:{}", input.port);
    let set = [
        ("CHALDEA_URL", url.clone()),
        ("CHALDEA_HOOK_URL", format!("{url}/hook")),
        ("CHALDEA_HOOK_TOKEN", input.hook_token.clone()),
        ("CHALDEA_THREAD_ID", input.thread_id.to_string()),
    ];
    env.extend(set.map(|(k, v)| (OsString::from(k), OsString::from(v))));
    if macos {
        env.push((OsString::from("TERM"), OsString::from(MACOS_TERM)));
    }
    env
}

/// `ws-` followed by the first 16 hex digits of the SHA-256 of `path`'s UTF-8 bytes
/// (`docs/mvp-design.md` §2.1): `path` is git's common directory, or the canonical cwd
/// outside git.
#[must_use]
pub fn workspace_id(path: &str) -> WorkspaceId {
    let digest = hex(&Sha256::digest(path.as_bytes()));
    WorkspaceId(format!("ws-{}", &digest[..16]))
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAUNCH: LaunchId = LaunchId::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    const THREAD: ThreadId = ThreadId::from_u128(7);

    fn exact(a: &OsStr, b: &OsStr) -> bool {
        a == b
    }

    fn exact_prefix(k: &OsStr, p: &OsStr) -> bool {
        k.as_encoded_bytes().starts_with(p.as_encoded_bytes())
    }

    /// The Windows rule: ordinal, case-insensitive.
    fn folded(a: &OsStr, b: &OsStr) -> bool {
        a.to_string_lossy().to_uppercase() == b.to_string_lossy().to_uppercase()
    }

    fn folded_prefix(k: &OsStr, p: &OsStr) -> bool {
        let (k, p) = (
            k.to_string_lossy().to_uppercase(),
            p.to_string_lossy().to_uppercase(),
        );
        k.starts_with(&p)
    }

    fn input(agent: Agent, os: HostOs) -> LaunchInput {
        LaunchInput {
            agent,
            host_os: os,
            thread_id: THREAD,
            launch_id: LAUNCH,
            resume_session: None,
            prompt: None,
            worktree_id: None,
            daemon_env: Vec::new(),
            key_eq: exact,
            key_has_prefix: exact_prefix,
            port: 4100,
            hook_token: "chh_token".to_owned(),
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    /// PR10a1-CL-1: an accepted prompt is the argv's last element, after a `--`, unsplit,
    /// whatever it looks like.
    #[test]
    fn codex_argv() {
        for prompt in ["--version", "-c x", "text with spaces\nand LF", "日本語 😀"] {
            for os in [HostOs::Macos, HostOs::Windows] {
                let mut i = input(Agent::Codex, os);
                i.prompt = Some(prompt.to_owned());
                let p = plan(&i);
                let n = p.args.len();
                assert_eq!(&p.args[n - 2..], ["--", prompt], "{os:?}");
                assert_eq!(p.args.iter().filter(|a| *a == "--").count(), 1);
                assert!(!p.manual);
            }
        }
        let mut i = input(Agent::Codex, HostOs::Macos);
        i.prompt = Some("hi".to_owned());
        assert_eq!(plan(&i).args, s(&["--no-daemon", "--", "hi"]));
        i.resume_session = Some("sess-1".to_owned());
        assert_eq!(
            plan(&i).args,
            s(&["resume", "--no-daemon", "sess-1", "--", "hi"])
        );
        i.prompt = None;
        let p = plan(&i);
        assert_eq!(p.args, s(&["resume", "--no-daemon", "sess-1"]));
        assert!(p.manual, "Codex without a prompt is manual");
    }

    /// PR10a1-CL-2: the OS-dependent argument comes from the mapped `HostOs`, on every
    /// runner.
    #[test]
    fn windows_flag_from_host_os() {
        let mut i = input(Agent::Codex, HostOs::Windows);
        i.prompt = Some("p".to_owned());
        assert_eq!(
            plan(&i).args,
            s(&[
                "--no-daemon",
                "-c",
                "tui.disable_paste_burst=true",
                "--",
                "p"
            ])
        );
        i.resume_session = Some("sess".to_owned());
        assert_eq!(
            plan(&i).args,
            s(&[
                "resume",
                "--no-daemon",
                "-c",
                "tui.disable_paste_burst=true",
                "sess",
                "--",
                "p"
            ])
        );
        let mut i = input(Agent::Codex, HostOs::Macos);
        i.prompt = Some("p".to_owned());
        assert!(!plan(&i).args.iter().any(|a| a.contains("paste_burst")));
    }

    /// PR10a1-CL-6 (argv part): Claude's session flag is the launch id (lowercase
    /// hyphenated, as `expected_session` computes it) on a fresh launch and the bound session
    /// on a resume; no prompt is ever in a Claude argv; Claude is never manual.
    #[test]
    fn claude_session_args() {
        let mut i = input(Agent::Claude, HostOs::Macos);
        let p = plan(&i);
        assert_eq!(
            p.args,
            s(&["--session-id", "01234567-89ab-cdef-0123-456789abcdef"])
        );
        assert!(!p.manual);
        i.resume_session = Some("bound-session".to_owned());
        i.prompt = Some("never in argv".to_owned());
        let p = plan(&i);
        assert_eq!(p.args, s(&["--resume", "bound-session"]));
        assert!(!p.manual);
    }

    fn get<'a>(env: &'a [(OsString, OsString)], k: &str) -> Vec<&'a OsString> {
        env.iter()
            .filter(|(key, _)| key == OsStr::new(k))
            .map(|(_, v)| v)
            .collect()
    }

    fn daemon_env() -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = ENV_SCRUB
            .iter()
            .map(|e| match e {
                Scrub::Name(n) => (OsString::from(*n), OsString::from("x")),
                Scrub::Prefix(p) => (OsString::from(format!("{p}AGENT_TOKEN")), "x".into()),
            })
            .collect();
        for (k, v) in [
            ("Chaldea_Hook_Token", "stale"),
            ("claudecode", "1"),
            ("CODEX_HOME", "/work/codex"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("PATH", "/usr/bin"),
            ("TERM", "screen"),
        ] {
            env.push((k.into(), v.into()));
        }
        env
    }

    /// PR10a1-CL-6 (environment part): every `ENV_SCRUB` name gone under the host's
    /// comparison, the user's other variables kept, and the four launch variables set once.
    #[test]
    fn env_scrub_and_launch_variables() {
        // macOS: exact comparison; the mixed-case names are other variables there.
        let mut i = input(Agent::Claude, HostOs::Macos);
        i.daemon_env = daemon_env();
        let env = plan(&i).env;
        for e in ENV_SCRUB {
            if let Scrub::Name(n) = e {
                assert!(get(&env, n).is_empty(), "{n}");
            }
        }
        assert!(get(&env, "CHALDEA_AGENT_TOKEN").is_empty());
        for (k, v) in [
            ("CODEX_HOME", "/work/codex"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("PATH", "/usr/bin"),
            ("Chaldea_Hook_Token", "stale"),
            ("claudecode", "1"),
            ("CHALDEA_URL", "http://127.0.0.1:4100"),
            ("CHALDEA_HOOK_URL", "http://127.0.0.1:4100/hook"),
            ("CHALDEA_HOOK_TOKEN", "chh_token"),
            ("CHALDEA_THREAD_ID", "00000000-0000-0000-0000-000000000007"),
            ("TERM", "xterm-256color"),
        ] {
            assert_eq!(get(&env, k), vec![&OsString::from(v)], "{k}");
        }
        // Windows: the comparison folds case, so the mixed-case names are scrubbed too (a
        // second `CHALDEA_HOOK_TOKEN` would refuse the spawn as a duplicate key), and the
        // daemon's `TERM` passes unchanged.
        let mut i = input(Agent::Codex, HostOs::Windows);
        i.daemon_env = daemon_env();
        i.key_eq = folded;
        i.key_has_prefix = folded_prefix;
        let env = plan(&i).env;
        assert!(get(&env, "Chaldea_Hook_Token").is_empty());
        assert!(get(&env, "claudecode").is_empty());
        assert_eq!(get(&env, "TERM"), vec![&OsString::from("screen")]);
        assert_eq!(
            get(&env, "CODEX_HOME"),
            vec![&OsString::from("/work/codex")]
        );
        let tokens = env
            .iter()
            .filter(|(k, _)| folded(k, OsStr::new("CHALDEA_HOOK_TOKEN")))
            .count();
        assert_eq!(tokens, 1);
    }

    #[test]
    fn argv_digest_covers_every_argument() {
        let mut i = input(Agent::Codex, HostOs::Macos);
        i.prompt = Some("a".to_owned());
        let a = plan(&i).argv_digest;
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        i.prompt = Some("b".to_owned());
        assert_ne!(plan(&i).argv_digest, a);
        // The separator keeps `["ab", "c"]` and `["a", "bc"]` apart.
        assert_ne!(argv_digest(&s(&["ab", "c"])), argv_digest(&s(&["a", "bc"])));
    }

    #[test]
    fn workspace_id_is_a_path_digest() {
        assert_eq!(workspace_id("/work/repo").as_str(), "ws-ddc5e473a09bd156");
        assert_ne!(workspace_id("/work/repo/"), workspace_id("/work/repo"));
    }
}
