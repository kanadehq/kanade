//! `kanade query` — run an ad-hoc **read-only** SQL query against the
//! backend's projector DB and print the result as a table (or raw JSON).
//!
//! HTTP-only subcommand: it POSTs to `/api/query` (admin-only, see
//! `kanade-backend::api::query`) using the bearer token from
//! `$KANADE_AUTH_TOKEN`. The server enforces SELECT/WITH-only, a row cap,
//! and a read-only connection — this is just the operator-facing front
//! end.

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

#[derive(Args, Debug)]
pub struct QueryArgs {
    /// The SQL to run. Read-only only (SELECT / WITH); the server rejects
    /// writes, DDL, stacked statements, and ATTACH/PRAGMA.
    pub sql: String,

    /// Max rows to return (server clamps to its own ceiling).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Print the raw JSON response instead of a rendered table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct QueryBody<'a> {
    sql: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
}

pub async fn execute(backend_url: &str, args: QueryArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    let url = format!("{base}/api/query");
    let resp = crate::http_client::authed_client()?
        .post(&url)
        .json(&QueryBody {
            sql: &args.sql,
            limit: args.limit,
        })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("query failed: {status} — {body}");
    }

    let payload: serde_json::Value = resp.json().await.context("decode response")?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    render_table(&payload);
    Ok(())
}

/// Render `{ columns, rows, row_count, truncated, elapsed_ms }` as an
/// aligned text table with a summary footer. Falls back to printing the
/// raw JSON if the shape is unexpected.
fn render_table(payload: &serde_json::Value) {
    let columns: Vec<String> = payload
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().map(cell_to_string).collect())
        .unwrap_or_default();
    let rows: Vec<Vec<String>> = payload
        .get("rows")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .map(|row| {
                    row.as_array()
                        .map(|cells| cells.iter().map(cell_to_string).collect())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();

    if columns.is_empty() {
        println!("(no rows)");
    } else {
        // Column widths = max of header + every cell in that column.
        let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(cell.chars().count());
                }
            }
        }
        let line = |cells: &[String]| {
            cells
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{:<width$}", c, width = widths.get(i).copied().unwrap_or(0)))
                .collect::<Vec<_>>()
                .join("  ")
        };
        println!("{}", line(&columns));
        println!(
            "{}",
            widths
                .iter()
                .map(|w| "-".repeat(*w))
                .collect::<Vec<_>>()
                .join("  ")
        );
        for row in &rows {
            println!("{}", line(row));
        }
    }

    let row_count = payload
        .get("row_count")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let truncated = payload
        .get("truncated")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let elapsed = payload
        .get("elapsed_ms")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let trunc = if truncated {
        " (truncated — raise --limit)"
    } else {
        ""
    };
    eprintln!("\n{row_count} row(s) in {elapsed} ms{trunc}");
}

/// Stringify a JSON cell for the table: strings raw (no quotes), null as
/// empty, everything else via its compact JSON form.
fn cell_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}
