//! Process-level tests for the `--color` policy: help, usage errors and an
//! offline human-facing command, with stdout and stderr piped (never a TTY).

use std::process::{Command, Output};

fn kanade(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_kanade"));
    c.env("KANADE_NO_AUTOUPDATE", "1")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("KANADE_COLOR")
        .args(args);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn has_ansi(s: &str) -> bool {
    s.contains('\x1b')
}

fn job_manifest() -> String {
    format!(
        "{}/../../configs/jobs/check-disk-space.yaml",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn piped_help_is_plain_by_default_and_colored_when_forced() {
    assert!(!has_ansi(&text(&kanade(&["--help"], &[]).stdout)));
    assert!(has_ansi(&text(
        &kanade(&["--color", "always", "--help"], &[]).stdout
    )));
    assert!(has_ansi(&text(
        &kanade(&["--help", "--color=always"], &[]).stdout
    )));
    assert!(has_ansi(&text(
        &kanade(&["job", "--color=always", "--help"], &[]).stdout
    )));
}

#[test]
fn never_beats_environment_force_and_always_beats_no_color() {
    let forced = [("CLICOLOR_FORCE", "1")];
    assert!(has_ansi(&text(&kanade(&["--help"], &forced).stdout)));
    assert!(!has_ansi(&text(
        &kanade(&["--color=never", "--help"], &forced).stdout
    )));
    assert!(has_ansi(&text(
        &kanade(&["--color=always", "--help"], &[("NO_COLOR", "1")]).stdout
    )));
    assert!(!has_ansi(&text(
        &kanade(&["--help"], &[("NO_COLOR", "1")]).stdout
    )));
}

#[test]
fn env_policy_is_overridden_by_the_flag() {
    let env = [("KANADE_COLOR", "always")];
    assert!(has_ansi(&text(&kanade(&["--help"], &env).stdout)));
    assert!(!has_ansi(&text(
        &kanade(&["--color=never", "--help"], &env).stdout
    )));
}

#[test]
fn usage_errors_follow_the_policy_on_stderr() {
    let colored = kanade(&["--color=always", "no-such-command"], &[]);
    assert_eq!(colored.status.code(), Some(2));
    assert!(has_ansi(&text(&colored.stderr)));
    let plain = kanade(&["no-such-command"], &[]);
    assert_eq!(plain.status.code(), Some(2));
    assert!(!has_ansi(&text(&plain.stderr)));
    let bad = kanade(&["--color=bogus", "job", "list"], &[]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn human_output_strips_back_to_the_plain_output() {
    let m = job_manifest();
    let plain = kanade(&["job", "validate", &m], &[]);
    let colored = kanade(&["--color=always", "job", "validate", &m], &[]);
    assert!(plain.status.success(), "{}", text(&plain.stderr));
    assert_eq!(plain.status.code(), colored.status.code());
    let (p, c) = (text(&plain.stdout), text(&colored.stdout));
    assert!(p.starts_with("✓ "), "{p}");
    assert!(!has_ansi(&p));
    assert!(has_ansi(&c));
    // Strip CSI sequences: what remains must equal the plain rendering.
    let mut stripped = String::new();
    let mut it = c.chars();
    while let Some(ch) = it.next() {
        if ch == '\x1b' {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            stripped.push(ch);
        }
    }
    assert_eq!(stripped, p);
}

#[test]
fn failure_marks_stay_on_stderr_and_exit_code_is_kept() {
    let missing = "definitely-missing.yaml";
    let plain = kanade(&["job", "validate", missing], &[]);
    let colored = kanade(&["--color=always", "job", "validate", missing], &[]);
    assert!(!plain.status.success());
    assert_eq!(plain.status.code(), colored.status.code());
    assert!(text(&plain.stdout).is_empty() && text(&colored.stdout).is_empty());
    assert!(!has_ansi(&text(&plain.stderr)));
}
