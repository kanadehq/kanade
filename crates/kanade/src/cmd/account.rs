//! `kanade account` — admin-only RBAC account management against the
//! backend `/api/accounts` endpoints. Requires `KANADE_AUTH_TOKEN` to
//! carry an admin token (or the static service token).

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::cmd::login::read_password;

#[derive(Args, Debug)]
pub struct AccountArgs {
    #[command(subcommand)]
    pub sub: AccountSub,
}

#[derive(Subcommand, Debug)]
pub enum AccountSub {
    /// List every account (no password hashes).
    List,
    /// Create an account. Password is prompted unless `--password-stdin`.
    Create {
        username: String,
        /// viewer | operator | admin
        #[arg(long)]
        role: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Change an account's role.
    SetRole {
        username: String,
        /// viewer | operator | admin
        role: String,
    },
    /// Reset an account's password (forces a change on next login).
    ResetPassword {
        username: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Disable an account (immediately blocks its existing tokens).
    Disable { username: String },
    /// Re-enable a disabled account.
    Enable { username: String },
    /// Delete an account.
    Delete { username: String },
}

pub async fn execute(backend_url: &str, args: AccountArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        AccountSub::List => list(base).await,
        AccountSub::Create {
            username,
            role,
            password_stdin,
        } => {
            let password = read_password(&format!("Password for {username}: "), password_stdin)?;
            patch_or_post(
                base,
                Method::Post,
                "/api/accounts",
                serde_json::json!({ "username": username, "password": password, "role": role }),
                &format!("created {username} ({role})"),
            )
            .await
        }
        AccountSub::SetRole { username, role } => {
            patch_or_post(
                base,
                Method::Patch,
                &format!("/api/accounts/{username}"),
                serde_json::json!({ "role": role }),
                &format!("{username} role -> {role}"),
            )
            .await
        }
        AccountSub::ResetPassword {
            username,
            password_stdin,
        } => {
            let password =
                read_password(&format!("New password for {username}: "), password_stdin)?;
            patch_or_post(
                base,
                Method::Patch,
                &format!("/api/accounts/{username}"),
                serde_json::json!({ "password": password }),
                &format!("{username} password reset (must change on next login)"),
            )
            .await
        }
        AccountSub::Disable { username } => {
            patch_or_post(
                base,
                Method::Patch,
                &format!("/api/accounts/{username}"),
                serde_json::json!({ "disabled": true }),
                &format!("{username} disabled"),
            )
            .await
        }
        AccountSub::Enable { username } => {
            patch_or_post(
                base,
                Method::Patch,
                &format!("/api/accounts/{username}"),
                serde_json::json!({ "disabled": false }),
                &format!("{username} enabled"),
            )
            .await
        }
        AccountSub::Delete { username } => delete(base, &username).await,
    }
}

enum Method {
    Post,
    Patch,
}

async fn list(base: &str) -> Result<()> {
    let url = format!("{base}/api/accounts");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("list failed: {status} — {body}");
    }
    let payload: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

async fn patch_or_post(
    base: &str,
    method: Method,
    path: &str,
    body: serde_json::Value,
    ok_msg: &str,
) -> Result<()> {
    let url = format!("{base}{path}");
    let client = crate::http_client::authed_client()?;
    let req = match method {
        Method::Post => client.post(&url),
        Method::Patch => client.patch(&url),
    };
    let resp = req
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("request failed: {status} — {body}");
    }
    println!("{ok_msg}");
    Ok(())
}

async fn delete(base: &str, username: &str) -> Result<()> {
    let url = format!("{base}/api/accounts/{username}");
    let resp = crate::http_client::authed_client()?
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("delete failed: {status} — {body}");
    }
    println!("deleted {username}");
    Ok(())
}
