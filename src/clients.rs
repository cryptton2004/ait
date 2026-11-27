//! Per-PC client tokens for the LAN mode: `ait remote add|list|revoke`.
//! Run on the host only. A token is shown once; only its SHA-256 is kept.

use crate::args::Args;
use crate::db::{self, Env};
use crate::error::{AitError, Result};
use crate::output::Output;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// Client names: 1-32 characters of letters, digits, `.`, `_`, `-`.
pub fn valid_name(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn token_hash(token: &str) -> Result<String> {
    Ok(crate::tls::hex(&crate::tls::sha256(
        token.trim().as_bytes(),
    )?))
}

pub fn cmd_remote(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    let sub = a.req_pos(0, "remote add|list|revoke")?;
    match sub {
        "add" => add(c, env, a),
        "list" | "ls" => list(c, a),
        "revoke" => revoke(c, env, a),
        other => Err(AitError::usage(format!(
            "unknown remote command '{other}' (use add, list or revoke)"
        ))),
    }
}

fn name_arg(a: &Args) -> Result<&str> {
    let name = a.req_pos(1, "client name (for example pc2)")?;
    if !valid_name(name) {
        return Err(AitError::new(
            "INVALID_INPUT",
            format!("client name '{name}' must be 1-32 letters, digits, '.', '_' or '-'"),
        ));
    }
    Ok(name)
}

fn add(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&[], 2)?;
    let name = name_arg(a)?;
    let token = crate::tls::random_hex(16)?;
    let hash = token_hash(&token)?;
    db::write(c, |c| {
        let revoked: Option<Option<i64>> = c
            .query_row("SELECT revoked FROM clients WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(None) = revoked {
            return Err(AitError::new(
                "IN_USE",
                format!(
                    "client '{name}' already has a token; revoke it first (ait remote revoke {name})"
                ),
            ));
        }
        c.execute(
            "INSERT INTO clients(name, token_hash, created, revoked) VALUES (?1, ?2, ?3, NULL)
             ON CONFLICT(name) DO UPDATE SET token_hash = excluded.token_hash,
                 created = excluded.created, revoked = NULL",
            params![name, hash, env.now],
        )?;
        db::event(c, env, None, "remote_add", json!({"client": name}))?;
        Ok(())
    })?;
    Ok(Output::new(
        json!({"client": name, "token": token}),
        format!("token for '{name}' (shown once; put it in that PC's token file):\n{token}"),
    ))
}

fn list(c: &Connection, a: &Args) -> Result<Output> {
    a.expect(&[], 1)?;
    let rows: Vec<Value> = db::read(c, |c| {
        let mut st = c.prepare("SELECT name, created, revoked FROM clients ORDER BY name")?;
        let rows = st
            .query_map([], |r| {
                let revoked: Option<i64> = r.get(2)?;
                Ok(json!({
                    "name": r.get::<_, String>(0)?,
                    "created": crate::time::iso(r.get(1)?),
                    "revoked": crate::time::iso_opt(revoked),
                    "active": revoked.is_none(),
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    let text = if rows.is_empty() {
        "no remote clients (add one with: ait remote add <name>)".to_string()
    } else {
        rows.iter()
            .map(|r| {
                let state = match r["revoked"].as_str() {
                    Some(t) => format!("revoked {t}"),
                    None => "active".to_string(),
                };
                format!(
                    "{}  created {}  {state}",
                    crate::output::s(r, "name"),
                    crate::output::s(r, "created")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Output::new(json!({"clients": rows}), text))
}

fn revoke(c: &Connection, env: &Env, a: &Args) -> Result<Output> {
    a.expect(&[], 2)?;
    let name = name_arg(a)?;
    db::write(c, |c| {
        let n = c.execute(
            "UPDATE clients SET revoked = ?2 WHERE name = ?1 AND revoked IS NULL",
            params![name, env.now],
        )?;
        if n == 0 {
            return Err(AitError::new(
                "NOT_FOUND",
                format!("no active client named '{name}'"),
            ));
        }
        db::event(c, env, None, "remote_revoke", json!({"client": name}))?;
        Ok(())
    })?;
    Ok(Output::new(
        json!({"client": name, "revoked": true}),
        format!("token for '{name}' revoked"),
    ))
}

/// The client a token belongs to: `Ok(name)` if active, `Err(REMOTE_AUTH)`
/// if unknown or revoked (the flag says which, for the HTTP status).
pub fn authenticate(c: &Connection, token: &str) -> std::result::Result<String, (bool, AitError)> {
    let auth = |m: &str| AitError::new("REMOTE_AUTH", m.to_string());
    let token = token.trim();
    if token.is_empty() {
        return Err((false, auth("missing client token (X-Ait-Client-Token)")));
    }
    let hash = token_hash(token).map_err(|e| (false, e))?;
    let row: Option<(String, Option<i64>)> = c
        .query_row(
            "SELECT name, revoked FROM clients WHERE token_hash = ?1",
            [hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| (false, AitError::from(e)))?;
    match row {
        None => Err((false, auth("unknown client token"))),
        Some((name, Some(_))) => Err((
            true,
            auth(&format!("the client token for '{name}' was revoked")),
        )),
        Some((name, None)) => Ok(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_names() {
        assert!(valid_name("pc2"));
        assert!(valid_name("build-box.1"));
        assert!(!valid_name(""));
        assert!(!valid_name("has space"));
        assert!(!valid_name(&"x".repeat(33)));
    }
}
