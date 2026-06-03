//! `kanade login` — exchange credentials for a JWT against the backend
//! `POST /api/auth/login`.
//!
//! The token is printed (and, with `--token-only`, printed *bare* so it
//! can be captured directly):
//!
//! ```sh
//! export KANADE_AUTH_TOKEN=$(kanade login --username alice --token-only)
//! ```
//!
//! `http_client::authed_client` then attaches it to every subsequent
//! `kanade exec` / `kanade account` / … request.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Account username. Prompted interactively when omitted.
    #[arg(long)]
    pub username: Option<String>,

    /// Read the password from stdin (one line) instead of prompting —
    /// for automation: `echo $PW | kanade login --username svc --password-stdin`.
    #[arg(long)]
    pub password_stdin: bool,

    /// Print only the raw token (no labels), for `export
    /// KANADE_AUTH_TOKEN=$(kanade login … --token-only)`.
    #[arg(long)]
    pub token_only: bool,
}

#[derive(Deserialize)]
struct LoginResp {
    token: String,
    role: String,
    must_change_pw: bool,
    exp: i64,
}

pub async fn execute(backend_url: &str, args: LoginArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    let username = match args.username {
        Some(u) => u,
        None => read_line("Username: ")?,
    };
    let password = read_password("Password: ", args.password_stdin)?;

    let url = format!("{base}/api/auth/login");
    let resp = crate::http_client::authed_client()?
        .post(&url)
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("login failed: {status} — {body}");
    }
    let payload: LoginResp = resp.json().await.context("decode login response")?;

    if args.token_only {
        println!("{}", payload.token);
        return Ok(());
    }

    println!("{}", payload.token);
    eprintln!(
        "role={} exp={} (set KANADE_AUTH_TOKEN to this token for subsequent commands)",
        payload.role, payload.exp,
    );
    if payload.must_change_pw {
        eprintln!(
            "NOTE: this account is flagged must-change-password — set a new one in the SPA or via `kanade account reset-password`.",
        );
    }
    Ok(())
}

/// Prompt + read a single trimmed line from stdin.
pub fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).context("read stdin")?;
    Ok(s.trim().to_string())
}

/// Read a password, either from stdin (one line, for piping) or via a
/// no-echo terminal prompt.
pub fn read_password(prompt: &str, from_stdin: bool) -> Result<String> {
    if from_stdin {
        let mut s = String::new();
        std::io::stdin()
            .read_line(&mut s)
            .context("read password from stdin")?;
        Ok(s.trim_end_matches(['\r', '\n']).to_string())
    } else {
        rpassword::prompt_password(prompt).context("read password prompt")
    }
}
