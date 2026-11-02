// ===========================================================================
// server.rs — client for the OpenCode background service's HTTP API.
//
// OpenCode v2 keeps a background service running (`opencode serve --service`,
// auto-started by any v2 command) that advertises itself through a
// registration file under `$XDG_STATE_HOME/opencode/service.json`:
//
//     { "id": ..., "version": "...", "url": "http://127.0.0.1:49374",
//       "pid": 1234, "password": "..." }
//
// Upstream treats that file as the complete discovery contract — reading it is
// all a client needs to connect (HTTP Basic auth with the fixed username
// `opencode` and the file's password). This module mirrors that:
//
//   resolve_endpoint()  --url flag > $MDRV_OC_SERVER_URL > registration file
//   projects()/sessions()  — read-only listing (the old Nushell
//                            `opencode-sessions`, absorbed)
//   prompt()/switch_agent()/switch_model() — delivery (the old
//                            `opencode-prompt`, absorbed)
//
// Endpoint paths verified against opencode v2.2.5
// (packages/protocol/src/groups/{session,project}.ts).
// ===========================================================================

use crate::error::{Error, Result};
use std::path::PathBuf;

/// Where to send requests: a base URL plus the Basic-auth password (the
/// username is fixed to `opencode` upstream).
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Base URL, no trailing slash — e.g. `http://127.0.0.1:49374`.
    pub base: String,
    /// Basic-auth password; `None` when the target needs no auth (or when a
    /// remote target is expected to accept unauthenticated requests).
    pub password: Option<String>,
}

impl Endpoint {
    fn auth_header(&self) -> Option<String> {
        self.password
            .as_deref()
            .map(|pw| format!("Basic {}", base64(format!("opencode:{pw}").as_bytes())))
    }
}

/// One project row from `GET /api/project` (subset we display).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Project {
    pub id: String,
    /// The project's canonical working directory.
    pub canonical: String,
}

/// One session row from `GET /api/session` (subset we display).
#[derive(Debug, Clone)]
pub struct ServerSession {
    pub id: String,
    pub title: Option<String>,
    pub directory: Option<String>,
    /// Session last-update time in epoch milliseconds (0 when absent).
    pub updated_ms: i64,
}

/// Resolve where to talk to OpenCode: explicit `--url`, then
/// `$MDRV_OC_SERVER_URL`, then the running service's registration file.
///
/// For explicit URLs the password comes from `$MDRV_OC_SERVER_PASSWORD`
/// (needed only for remote targets; local discovery reads it from the file).
pub fn resolve_endpoint(url_override: Option<&str>) -> Result<Endpoint> {
    if let Some(url) = url_override {
        return Ok(Endpoint {
            base: url.trim_end_matches('/').to_owned(),
            password: std::env::var("MDRV_OC_SERVER_PASSWORD")
                .ok()
                .filter(|p| !p.is_empty()),
        });
    }
    if let Some(url) = std::env::var("MDRV_OC_SERVER_URL")
        .ok()
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
    {
        return Ok(Endpoint {
            base: url.trim_end_matches('/').to_owned(),
            password: std::env::var("MDRV_OC_SERVER_PASSWORD")
                .ok()
                .filter(|p| !p.is_empty()),
        });
    }
    let reg = read_registration()?.ok_or_else(|| Error::InvalidInput {
        msg: "no OpenCode server found: no --url, no $MDRV_OC_SERVER_URL, and no \
              running background service (start one with `opencode serve` or any \
              opencode command)"
            .into(),
    })?;
    Ok(Endpoint {
        base: reg.0,
        password: reg.1,
    })
}

/// ($url, $password) from `$XDG_STATE_HOME/opencode/service.json`, or `None`
/// when the file is missing/unreadable/unparseable (a stopped or ancient
/// service — callers fall back to a clear error).
fn read_registration() -> Result<Option<(String, Option<String>)>> {
    let Some(path) = registration_path() else {
        return Ok(None);
    };
    read_registration_from(&path)
}

fn read_registration_from(path: &std::path::Path) -> Result<Option<(String, Option<String>)>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let Some(url) = v.get("url").and_then(|u| u.as_str()) else {
        return Ok(None);
    };
    let password = v
        .get("password")
        .and_then(|p| p.as_str())
        .map(str::to_owned);
    Ok(Some((url.trim_end_matches('/').to_owned(), password)))
}

/// Path of the service's registration file, mirroring upstream's own
/// resolution: `$XDG_STATE_HOME` (absolute) else `$HOME/.local/state` (with
/// the same `%USERPROFILE%` fallback opencode uses), then `/opencode/service.json`.
fn registration_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|s| PathBuf::from(s).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            crate::pathutil::home_dir()
                .map(PathBuf::from)
                .map(|h| h.join(".local/state"))
        })?;
    Some(state.join("opencode").join("service.json"))
}

// ---------------------------------------------------------------------------
// Read-only API
// ---------------------------------------------------------------------------

/// `GET /api/project` — all known projects (upstream returns a bare array).
pub fn projects(ep: &Endpoint) -> Result<Vec<Project>> {
    let v = request_json(ep, "GET", "/api/project", None, None)?;
    let arr = as_array(&v).ok_or_else(|| Error::InvalidInput {
        msg: "opencode server returned a non-array project list".into(),
    })?;
    Ok(arr
        .iter()
        .filter_map(|p| {
            Some(Project {
                id: p.get("id")?.as_str()?.to_owned(),
                canonical: p
                    .get("canonical")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect())
}

/// `GET /api/session` — sessions, newest first, across every project the
/// server knows. (Empirically, the running v2.2.x service scopes this
/// endpoint to its whole project set and ignores both the
/// `x-opencode-directory` header and the `location[directory]` query — which
/// is fine: one global call is exactly what the absorbed Nushell
/// `opencode-sessions` sharded by hand. Directory narrowing happens
/// client-side, see `sessions_in`.)
pub fn sessions(ep: &Endpoint, limit: u32) -> Result<Vec<ServerSession>> {
    let path = format!("/api/session?limit={limit}&order=desc");
    let v = request_json(ep, "GET", &path, None, None)?;
    let arr = v
        .get("data")
        .and_then(|d| as_array(d))
        .or_else(|| as_array(&v))
        .ok_or_else(|| Error::InvalidInput {
            msg: "opencode server returned an unrecognized session list".into(),
        })?;
    Ok(arr
        .iter()
        .filter_map(|s| {
            let id = s.get("id")?.as_str()?.to_owned();
            let title = s.get("title").and_then(|t| t.as_str()).map(str::to_owned);
            let directory = s
                .pointer("/location/directory")
                .or_else(|| s.get("directory"))
                .and_then(|d| d.as_str())
                .map(str::to_owned);
            let updated_ms = s
                .pointer("/time/updated")
                .or_else(|| s.pointer("/time/created"))
                .and_then(|t| t.as_i64())
                .unwrap_or(0);
            Some(ServerSession {
                id,
                title,
                directory,
                updated_ms,
            })
        })
        .collect())
}

/// Sessions restricted to one project directory (client-side filter; a
/// trailing slash on either side is ignored).
pub fn sessions_in(ep: &Endpoint, directory: &str, limit: u32) -> Result<Vec<ServerSession>> {
    fn norm(p: &str) -> &str {
        p.trim_end_matches('/')
    }
    Ok(sessions(ep, limit)?
        .into_iter()
        .filter(|s| {
            s.directory
                .as_deref()
                .map(|d| norm(d) == norm(directory))
                .unwrap_or(false)
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Delivery API (the absorbed `opencode-prompt`)
// ---------------------------------------------------------------------------

/// `POST /api/session/:id/agent` — switch the session's agent.
pub fn switch_agent(ep: &Endpoint, session_id: &str, agent: &str) -> Result<()> {
    request_json(
        ep,
        "POST",
        &format!("/api/session/{session_id}/agent"),
        Some(&serde_json::json!({ "agent": agent })),
        None,
    )?;
    Ok(())
}

/// `POST /api/session/:id/model` — switch the session's model.
pub fn switch_model(ep: &Endpoint, session_id: &str, provider: &str, model: &str) -> Result<()> {
    request_json(
        ep,
        "POST",
        &format!("/api/session/{session_id}/model"),
        Some(&serde_json::json!({ "model": { "providerID": provider, "id": model } })),
        None,
    )?;
    Ok(())
}

/// `POST /api/session/:id/prompt` — deliver the prompt text. Upstream parses
/// `{ "text": ... }` (`PromptInput`); while the session is busy the prompt is
/// parked/steered, so scheduling into a running session is safe.
pub fn prompt(ep: &Endpoint, session_id: &str, text: &str) -> Result<()> {
    request_json(
        ep,
        "POST",
        &format!("/api/session/{session_id}/prompt"),
        Some(&serde_json::json!({ "text": text })),
        None,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP plumbing (ureq, rustls)
// ---------------------------------------------------------------------------

fn request_json(
    ep: &Endpoint,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    extra_header: Option<(&str, &str)>,
) -> Result<serde_json::Value> {
    let url = format!("{}{path}", ep.base);
    let label = format!("opencode-server {method} {path}");
    let auth = ep.auth_header();
    // One agent per request is fine at our volumes; the config carries the
    // global timeout (request-level `.timeout()` doesn't exist in ureq 3).
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .new_agent();

    let response = match method {
        "GET" => {
            let mut r = agent.get(&url);
            if let Some(a) = &auth {
                r = r.header("Authorization", a.as_str());
            }
            if let Some((k, v)) = extra_header {
                r = r.header(k, v);
            }
            r.call()
        }
        _ => {
            let mut r = agent.post(&url);
            if let Some(a) = &auth {
                r = r.header("Authorization", a.as_str());
            }
            if let Some((k, v)) = extra_header {
                r = r.header(k, v);
            }
            let payload = body.map(|v| v.to_string()).unwrap_or_default();
            r.header("Content-Type", "application/json").send(payload)
        }
    }
    .map_err(|e| Error::External {
        cmd: label.clone(),
        detail: e.to_string(),
    })?;

    let status = response.status().as_u16();
    let mut response = response;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::External {
            cmd: label.clone(),
            detail: e.to_string(),
        })?;
    if !(200..300).contains(&status) {
        return Err(Error::External {
            cmd: label,
            detail: format!("HTTP {status}: {}", truncate(&text, 400)),
        });
    }
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| Error::External {
        cmd: label,
        detail: format!("non-JSON response: {} ({e})", truncate(&text, 200)),
    })
}

fn as_array(v: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    v.as_array()
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Standard base64 (with padding) — enough for one Basic-auth header, keeps
/// the dependency tree free of `base64`.
fn base64(input: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TBL[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TBL[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"opencode:pw"), "b3BlbmNvZGU6cHc=");
    }

    #[test]
    fn registration_parses_url_and_password() {
        let dir = std::env::temp_dir().join("mdrv-oc-test-server");
        std::fs::create_dir_all(dir.join("opencode")).unwrap();
        let path = dir.join("opencode/service.json");
        std::fs::write(
            &path,
            r#"{"id":"x","version":"2.2.5","url":"http://127.0.0.1:49374","pid":7,"password":"pw"}"#,
        )
        .unwrap();

        let reg = read_registration_from(&path)
            .unwrap()
            .expect("registration found");
        assert_eq!(reg.0, "http://127.0.0.1:49374");
        assert_eq!(reg.1.as_deref(), Some("pw"));

        // Missing file → None (not an error).
        assert!(
            read_registration_from(&dir.join("nowhere/service.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn endpoint_auth_header_uses_fixed_username() {
        let ep = Endpoint {
            base: "http://x".into(),
            password: Some("pw".into()),
        };
        assert_eq!(ep.auth_header().unwrap(), "Basic b3BlbmNvZGU6cHc=");
        let ep = Endpoint {
            base: "http://x".into(),
            password: None,
        };
        assert!(ep.auth_header().is_none());
    }
}
