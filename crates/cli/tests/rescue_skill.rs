//! T2-16 (`docs/design/t2-terminal.md` §6.3 `rescue_skill_commands`): every `chaldea …`
//! example in `skills/chaldea-rescue/SKILL.md` parses with the CLI's own clap definition, the
//! commands that can be pinned are shown pinned, and the skill names no URL, `curl`, or token
//! file.

use std::ffi::OsString;

const SKILL: &str = include_str!("../../../skills/chaldea-rescue/SKILL.md");

/// A value of each placeholder the skill uses.
fn value(placeholder: &str) -> Option<&'static str> {
    Some(match placeholder {
        "thread" => "00000000-0000-4000-8000-000000000001",
        "inspected" => "00000000-0000-4000-8000-000000000002",
        "input" => "00000000-0000-4000-8000-000000000003",
        "approval" => "00000000-0000-4000-8000-000000000004",
        "keys" => "down enter",
        "text" => "hello",
        "outcome" => "delivered",
        "agent" => "claude",
        "path" => "/work/repo",
        _ => return None,
    })
}

/// The inline code spans and fenced-block lines of `md`'s body (after its frontmatter, whose
/// description names commands without arguments) that start with `chaldea `.
fn examples(md: &str) -> Vec<String> {
    let body = md
        .strip_prefix("---\n")
        .and_then(|m| m.split_once("\n---\n"))
        .map(|(_, body)| body)
        .expect("the skill's frontmatter");
    let mut out = Vec::new();
    let mut fenced = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.push(line.trim().to_owned());
            continue;
        }
        out.extend(line.split('`').skip(1).step_by(2).map(str::to_owned));
    }
    out.retain(|e| e.starts_with("chaldea "));
    out
}

/// `example` with each `<name>` replaced by its value, as argv.
fn argv(example: &str) -> Vec<OsString> {
    let mut s = String::new();
    let mut rest = example;
    while let Some(i) = rest.find('<') {
        s.push_str(&rest[..i]);
        let end = rest[i..]
            .find('>')
            .unwrap_or_else(|| panic!("an open placeholder in `{example}`"));
        let name = &rest[i + 1..i + end];
        let v =
            value(name).unwrap_or_else(|| panic!("unknown placeholder <{name}> in `{example}`"));
        s.push_str(v);
        rest = &rest[i + end + 1..];
    }
    s.push_str(rest);
    s.split_whitespace().map(OsString::from).collect()
}

#[test]
fn rescue_skill_commands() {
    let examples = examples(SKILL);
    assert!(examples.len() >= 10, "{examples:?}");
    for e in &examples {
        if let Err(m) = chaldea_cli::check_args(&argv(e)) {
            panic!("`{e}` does not parse:\n{m}");
        }
    }
    // Every command that takes a pin is shown with it.
    for cmd in ["ready", "stop", "resume", "close", "key"] {
        let of: Vec<&String> = examples
            .iter()
            .filter(|e| e.split_whitespace().nth(1) == Some(cmd))
            .collect();
        assert!(!of.is_empty(), "no `chaldea {cmd}` example");
        for e in of {
            assert!(e.contains("--launch <inspected>"), "`{e}` is not pinned");
        }
    }
}

/// The check above, failing: an example of a flag the CLI does not have.
#[test]
fn an_unknown_flag_does_not_parse() {
    let bad = argv("chaldea ready <thread> --pin <inspected>");
    assert!(chaldea_cli::check_args(&bad).is_err());
}

#[test]
fn rescue_skill_names_no_api_access() {
    let lower = SKILL.to_lowercase();
    for banned in [
        "://",
        "curl",
        "/api/",
        "owner.token",
        "authorization",
        "bearer",
    ] {
        assert!(!lower.contains(banned), "the skill names `{banned}`");
    }
    // JSON argument examples would need a check against their schemas; the skill has none.
    assert!(
        !SKILL.contains("```json"),
        "a JSON example needs its schema check"
    );
}

/// Guards T2-4 (c): the agent pins the launch of the inspection the human's request refers to,
/// and hands back when a later `inspect` shows another launch, so a refreshed inspection never
/// retargets an authorized action to a replacement launch.
#[test]
fn rescue_skill_freezes_the_launch() {
    for line in [
        "`<inspected>` is frozen: take it, and any input id, from the inspection the human's request refers to",
        "If a later `inspect` shows a different launch, hand back",
    ] {
        assert!(SKILL.contains(line), "the skill lost: {line}");
    }
    assert!(
        !SKILL.contains("latest `inspect`"),
        "the pin comes from the request's inspection"
    );
}
