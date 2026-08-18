//! Keep Ocean's `auth.json` OAuth blocks fresh.
//!
//! Ocean previously only *read* OAuth tokens that external CLIs (Claude Code,
//! Codex) wrote — an expired block resolved to "missing credential" and the
//! turn hard-failed until the user re-logged-in elsewhere. This module runs at
//! the top of the turn path: for each known refreshable block in **Ocean's
//! own** `auth.json` (never `~/.codex/auth.json` or any other CLI's file) that
//! is expired or expiring inside the margin, it exchanges the refresh token at
//! the issuer's public-client token endpoint and rewrites the block atomically
//! (temp file + rename in the same directory — a crash never corrupts the
//! file other CLIs may share).
//!
//! Failure is graceful and rate-limited: a failed refresh logs, starts a
//! per-block cooldown (no hammering the endpoint every turn), and leaves
//! behavior exactly as pre-refresh (expired → missing credential).

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Refresh when a token expires within this margin — covers the whole turn so
/// a token can't expire mid-run.
const EXPIRY_MARGIN_SECS: i64 = 300;
/// After a failed refresh, don't retry that block for this long.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

/// One refreshable block: where it lives in auth.json and how to refresh it.
struct RefreshableBlock {
    /// Top-level key in auth.json.
    block: &'static str,
    /// Env var overriding the token endpoint (tests, region overrides).
    endpoint_env: &'static str,
    default_endpoint: &'static str,
    /// Env var overriding the OAuth client id.
    client_id_env: &'static str,
    /// The issuer's public-client id (the same one the vendor CLI itself
    /// embeds; refreshing the user's own tokens with it is the intended flow).
    default_client_id: &'static str,
}

const BLOCKS: &[RefreshableBlock] = &[
    RefreshableBlock {
        block: "claude-code",
        endpoint_env: "OCEAN_OAUTH_ANTHROPIC_TOKEN_URL",
        default_endpoint: "https://console.anthropic.com/v1/oauth/token",
        client_id_env: "OCEAN_OAUTH_ANTHROPIC_CLIENT_ID",
        default_client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    },
    RefreshableBlock {
        block: "anthropic-oauth",
        endpoint_env: "OCEAN_OAUTH_ANTHROPIC_TOKEN_URL",
        default_endpoint: "https://console.anthropic.com/v1/oauth/token",
        client_id_env: "OCEAN_OAUTH_ANTHROPIC_CLIENT_ID",
        default_client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    },
    RefreshableBlock {
        block: "openai-codex",
        endpoint_env: "OCEAN_OAUTH_OPENAI_TOKEN_URL",
        default_endpoint: "https://auth.openai.com/oauth/token",
        client_id_env: "OCEAN_OAUTH_OPENAI_CLIENT_ID",
        default_client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    },
];

/// Single-flight + per-block failure cooldowns. One global refresh pass runs
/// at a time (concurrent turns don't race the file or double-spend a rotating
/// refresh token); failed blocks are skipped until their cooldown lapses.
struct RefreshState {
    cooldowns: HashMap<String, Instant>,
}

fn state() -> &'static tokio::sync::Mutex<RefreshState> {
    static STATE: OnceLock<tokio::sync::Mutex<RefreshState>> = OnceLock::new();
    STATE.get_or_init(|| {
        tokio::sync::Mutex::new(RefreshState {
            cooldowns: HashMap::new(),
        })
    })
}

/// Belt-and-braces outer deadline on one token-endpoint exchange. The HTTP
/// client inside `ocean_protocol::oauth::refresh_token` already carries its own
/// 15s total timeout; this wrapper guarantees the *await itself* is bounded
/// even if a future client change (or a pathological resolver path) loses that
/// bound. Load-bearing because `ensure_fresh` sits at the top of EVERY turn:
/// an unbounded await here has fleet-wide blast radius (see the single-flight
/// note below).
const REFRESH_CALL_DEADLINE: Duration = Duration::from_secs(20);

/// Refresh every expiring refreshable block in `auth_file`. Cheap no-op when
/// the file is absent or every block is fresh. Never returns an error — a
/// refresh failure degrades to today's behavior (expired block → missing
/// credential) with a warning and a cooldown.
///
/// # Single-flight is SKIP, not QUEUE (daemon-freeze fix)
///
/// This function runs at the top of **every turn**. It used to take the global
/// state lock with `.lock().await`, which made the lock a process-wide choke
/// point: every concurrent turn *queued* behind whichever turn held the guard,
/// and the guard was held across a network await and blocking file I/O. If the
/// holder wedged (hung filesystem under `auth_file`, any unbounded await), every
/// subsequent turn on every session parked here forever — silently, with no
/// deadline and no log line: exactly the observed "all turns freeze at turn
/// start, log goes quiet, process looks idle" failure.
///
/// Now a turn that finds a refresh already in flight **skips** (`try_lock`) —
/// the same graceful degradation as a failed refresh: credential resolution
/// proceeds with whatever is on disk. At most one turn ever pays for (or is
/// exposed to) the refresh; a wedged refresh can strand only itself, never the
/// fleet. The single-flight guarantee that matters — never two concurrent
/// exchanges double-spending a rotating refresh token — is preserved.
pub async fn ensure_fresh(auth_file: &Path) {
    if !auth_file.exists() {
        return;
    }
    let Ok(mut guard) = state().try_lock() else {
        tracing::debug!(
            "OAuth refresh already in flight; proceeding with on-disk credentials"
        );
        return;
    };

    let Ok(raw) = std::fs::read_to_string(auth_file) else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<Value>(&raw) else {
        return; // malformed file: resolution will surface it; don't touch
    };

    let mut changed = false;
    for def in BLOCKS {
        let Some((refresh, needs)) = block_needs_refresh(&json, def.block) else {
            continue;
        };
        if !needs {
            continue;
        }
        if let Some(until) = guard.cooldowns.get(def.block) {
            if until.elapsed() < FAILURE_COOLDOWN {
                continue;
            }
        }
        let endpoint =
            std::env::var(def.endpoint_env).unwrap_or_else(|_| def.default_endpoint.to_string());
        let client_id =
            std::env::var(def.client_id_env).unwrap_or_else(|_| def.default_client_id.to_string());
        // Outer deadline (see REFRESH_CALL_DEADLINE): a refresh that outlives
        // it is treated exactly like a failed refresh — warn + cooldown — so
        // this await can never hold the single-flight guard unboundedly.
        let refreshed = match tokio::time::timeout(
            REFRESH_CALL_DEADLINE,
            ocean_protocol::oauth::refresh_token(&endpoint, &client_id, &refresh),
        )
        .await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(ocean_protocol::Error::InvalidResponse(format!(
                "token refresh exceeded the {}s outer deadline",
                REFRESH_CALL_DEADLINE.as_secs()
            ))),
        };
        match refreshed {
            Ok(fresh) => {
                let entry = json
                    .get_mut(def.block)
                    .and_then(Value::as_object_mut)
                    .expect("block existed above");
                entry.insert("access".into(), Value::String(fresh.access_token));
                if let Some(rt) = fresh.refresh_token {
                    entry.insert("refresh".into(), Value::String(rt));
                }
                if let Some(secs) = fresh.expires_in_secs {
                    let expires_ms = (unix_secs() + secs) * 1_000;
                    entry.insert("expires".into(), Value::from(expires_ms));
                }
                guard.cooldowns.remove(def.block);
                changed = true;
                tracing::info!(block = %def.block, "refreshed OAuth token");
            }
            Err(e) => {
                guard
                    .cooldowns
                    .insert(def.block.to_string(), Instant::now());
                tracing::warn!(
                    block = %def.block,
                    error = %e,
                    "OAuth refresh failed; leaving block as-is (60s cooldown)"
                );
            }
        }
    }

    if changed {
        if let Err(e) = write_atomically(auth_file, &json) {
            tracing::warn!(error = %e, "could not persist refreshed auth.json");
        }
    }
}

/// `Some((refresh_token, needs_refresh))` for an oauth block that HAS a
/// refresh token; `None` when the block is absent, non-oauth, or unrefreshable.
fn block_needs_refresh(json: &Value, block: &str) -> Option<(String, bool)> {
    let entry = json.get(block)?;
    if entry.get("type").and_then(Value::as_str) != Some("oauth") {
        return None;
    }
    let refresh = entry
        .get("refresh")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    // No known expiry → treat as fresh (nothing to gain by refreshing blind).
    let Some(expires) = entry.get("expires").and_then(Value::as_i64) else {
        return Some((refresh, false));
    };
    let expires_secs = if expires >= 1_000_000_000_000 {
        expires / 1_000
    } else {
        expires
    };
    Some((refresh, expires_secs <= unix_secs() + EXPIRY_MARGIN_SECS))
}

/// Write `json` to `path` via a temp file + rename in the same directory, so a
/// crash mid-write can never leave a truncated auth.json. The temp file and the
/// containing directory are fsynced so the replacement is durable across power
/// loss (see `crate::durable`).
fn write_atomically(path: &Path, json: &Value) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".auth.json.tmp-{}", std::process::id()));
    let pretty = serde_json::to_string_pretty(json).unwrap_or_else(|_| json.to_string());
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(pretty.as_bytes())?;
        file.sync_all()?;
    }
    crate::durable::durable_rename(&tmp, path)
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn needs_refresh_only_for_expiring_oauth_blocks_with_refresh_tokens() {
        let far = (unix_secs() + 86_400) * 1_000;
        let soon = (unix_secs() + 60) * 1_000;
        let j = json!({
            "claude-code": { "type": "oauth", "access": "a", "refresh": "r", "expires": soon },
            "fresh": { "type": "oauth", "access": "a", "refresh": "r", "expires": far },
            "no-refresh": { "type": "oauth", "access": "a", "expires": 1 },
            "api-key-block": { "api_key": "k" }
        });
        assert_eq!(
            block_needs_refresh(&j, "claude-code").map(|(_, n)| n),
            Some(true),
            "expiring-within-margin block must refresh"
        );
        assert_eq!(
            block_needs_refresh(&j, "fresh").map(|(_, n)| n),
            Some(false),
            "fresh block must not refresh"
        );
        assert!(
            block_needs_refresh(&j, "no-refresh").is_none(),
            "no refresh token → unrefreshable"
        );
        assert!(block_needs_refresh(&j, "api-key-block").is_none());
        assert!(block_needs_refresh(&j, "absent").is_none());
    }

    #[test]
    fn atomic_write_preserves_unrelated_keys() {
        let dir = std::env::temp_dir().join(format!("ocean-oauth-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        let mut j = json!({
            "claude-code": { "type": "oauth", "access": "old", "refresh": "r", "expires": 1 },
            "deepseek": { "api_key": "keep-me" }
        });
        j["claude-code"]["access"] = json!("new");
        write_atomically(&path, &j).unwrap();
        let round: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(round["claude-code"]["access"], "new");
        assert_eq!(
            round["deepseek"]["api_key"], "keep-me",
            "unrelated provider keys must survive the rewrite"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Daemon-freeze regression (single-flight = SKIP, not QUEUE): while one
    /// refresh pass holds the global state lock — stand-in for a wedged holder
    /// stuck in a network await or on a hung filesystem — every other turn's
    /// `ensure_fresh` must return immediately instead of parking on the lock.
    /// Before the fix this test would hang until the held guard was dropped:
    /// the exact interleaving that froze every turn on the fleet.
    #[tokio::test]
    async fn in_flight_refresh_never_queues_other_turns() {
        let dir = std::env::temp_dir().join(format!(
            "ocean-oauth-inflight-{}-{:x}",
            std::process::id(),
            unix_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            json!({
                "claude-code": { "type": "oauth", "access": "stale", "refresh": "rt", "expires": 1 }
            })
            .to_string(),
        )
        .unwrap();

        // Simulate an in-flight (possibly wedged) refresh holding the guard.
        let held = state().lock().await;

        let start = std::time::Instant::now();
        ensure_fresh(&path).await;
        let elapsed = start.elapsed();

        drop(held);

        assert!(
            elapsed < Duration::from_secs(2),
            "ensure_fresh must skip (not queue) while a refresh is in flight; \
             waited {elapsed:?}"
        );
        // The skipping turn must not have touched the file (no exchange ran).
        let round: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(round["claude-code"]["access"], "stale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end against a fake token endpoint: the expiring block is
    /// refreshed on disk (new access + rotated refresh + future expiry), the
    /// fresh block and unrelated keys are untouched.
    #[tokio::test]
    async fn ensure_fresh_rewrites_expiring_block_via_endpoint_override() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let body =
                        r#"{"access_token":"minty","refresh_token":"rotated","expires_in":3600}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let dir = std::env::temp_dir().join(format!("ocean-oauth-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            json!({
                "claude-code": { "type": "oauth", "access": "stale", "refresh": "old-rt", "expires": 1 },
                "deepseek": { "api_key": "keep-me" }
            })
            .to_string(),
        )
        .unwrap();

        // Endpoint override via env: serialized by the global single-flight
        // lock inside ensure_fresh, and this is the only test setting it.
        std::env::set_var(
            "OCEAN_OAUTH_ANTHROPIC_TOKEN_URL",
            format!("http://{addr}/oauth/token"),
        );
        ensure_fresh(&path).await;
        std::env::remove_var("OCEAN_OAUTH_ANTHROPIC_TOKEN_URL");

        let round: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(round["claude-code"]["access"], "minty");
        assert_eq!(round["claude-code"]["refresh"], "rotated");
        let exp = round["claude-code"]["expires"].as_i64().unwrap();
        assert!(exp > unix_secs() * 1_000, "expiry moved into the future");
        assert_eq!(round["deepseek"]["api_key"], "keep-me");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
