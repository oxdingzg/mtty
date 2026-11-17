//! Private-pipe administration of an existing miao Runtime.
//! Call on a background job. Never put requests or replies in logs: invitations
//! contain secrets and relay setup requests contain an account password.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Serialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum Operation {
    Status,
    Session {
        #[serde(rename = "sessionID")]
        session_id: String,
    },
    Invite {
        policy: Policy,
    },
    Pending,
    Devices,
    Approve {
        #[serde(rename = "pairingID")]
        pairing_id: String,
        #[serde(rename = "publicKey")]
        public_key: String,
    },
    Reject {
        #[serde(rename = "pairingID")]
        pairing_id: String,
    },
    Revoke {
        #[serde(rename = "grantID")]
        grant_id: String,
        #[serde(rename = "grantVersion")]
        grant_version: u64,
    },
    Setup {
        #[serde(rename = "hubURL")]
        hub_url: String,
        email: String,
        password: String,
        name: String,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub permissions: Vec<String>,
    #[serde(rename = "projectIDs")]
    pub project_ids: Vec<String>,
    #[serde(rename = "sessionIDs")]
    pub session_ids: Vec<String>,
    pub expires_at: i64,
    /// Owner opt-in: same-account devices are admitted without scanning.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_admit: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize)]
pub struct Request {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    #[serde(rename = "runtimeID", skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<String>,
    #[serde(flatten)]
    pub operation: Operation,
}

// Deliberately no Debug implementation on secret-bearing request/reply types.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReply {
    version: u8,
    ok: bool,
    #[serde(rename = "runtimeID")]
    runtime_id: Option<String>,
    data: Option<Value>,
    error: Option<String>,
}

pub struct Reply {
    pub runtime_id: String,
    pub data: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidRequest,
    NoRuntime,
    RuntimeChanged,
    Unavailable,
    /// Includes timeout/cancellation: inspect authoritative state before retry.
    Unconfirmed,
}

/// Run only a trusted executable. This never uses a shell or pane input.
/// All failures during mutations are conservatively uncertain; never retry them
/// automatically. `cancel` also ends a read without leaving an owned child alive.
pub fn call(executable: &Path, request: &Request, cancel: &AtomicBool) -> Result<Reply, Error> {
    call_with_deadline(executable, request, cancel, Duration::from_secs(120))
}

fn call_with_deadline(
    executable: &Path,
    request: &Request,
    cancel: &AtomicBool,
    budget: Duration,
) -> Result<Reply, Error> {
    let read = matches!(
        request.operation,
        Operation::Status | Operation::Session { .. } | Operation::Pending | Operation::Devices
    );
    let failure = || {
        if read {
            Error::Unavailable
        } else {
            Error::Unconfirmed
        }
    };
    if !matches!(request.operation, Operation::Status) && request.runtime_id.is_none() {
        return Err(Error::InvalidRequest);
    }
    let mut value = serde_json::to_value(request).map_err(|_| Error::InvalidRequest)?;
    value["version"] = Value::from(1);
    let bytes = serde_json::to_vec(&value).map_err(|_| Error::InvalidRequest)?;
    if bytes.len() > 65_536 {
        return Err(Error::InvalidRequest);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(failure());
    }
    let mut command = Command::new(executable);
    command
        .args(["runtime", "access"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|_| failure())?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let writer = std::thread::spawn(move || stdin.write_all(&bytes));
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let started = Instant::now();
    let outcome = loop {
        if cancel.load(Ordering::Relaxed) || started.elapsed() >= budget {
            break Err(failure());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err(failure())
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break Err(failure()),
        }
    };
    if outcome.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let wrote = writer
        .join()
        .map_err(|_| failure())
        .and_then(|result| result.map_err(|_| failure()));
    let output = reader
        .join()
        .map_err(|_| failure())
        .and_then(|result| result.map_err(|_| failure()));
    outcome?;
    wrote?;
    decode(&output?, request, failure())
}

fn decode(bytes: &[u8], request: &Request, failure: Error) -> Result<Reply, Error> {
    if bytes.len() > 1_048_576 {
        return Err(failure);
    }
    let wire: WireReply = serde_json::from_slice(bytes).map_err(|_| failure)?;
    if wire.version != 1 {
        return Err(failure);
    }
    if !wire.ok {
        return Err(match wire.error.as_deref() {
            Some("invalidRequest") => Error::InvalidRequest,
            Some("noRuntime") => Error::NoRuntime,
            Some("runtimeChanged") => Error::RuntimeChanged,
            Some("unavailable") => Error::Unavailable,
            _ => Error::Unconfirmed,
        });
    }
    let runtime_id = wire.runtime_id.ok_or(Error::Unconfirmed)?;
    if runtime_id.is_empty() || wire.error.is_some() {
        return Err(Error::Unconfirmed);
    }
    if request
        .runtime_id
        .as_ref()
        .is_some_and(|expected| expected != &runtime_id)
    {
        return Err(Error::RuntimeChanged);
    }
    if wire.data.is_none() && !matches!(request.operation, Operation::Reject { .. }) {
        return Err(failure);
    }
    // A successful rejection legitimately returns JSON null.
    Ok(Reply {
        runtime_id,
        data: wire.data.unwrap_or(Value::Null),
    })
}

/// The `(runtimeID, storage)` an observing client reported for its pane, when
/// that client owns the Runtime. Attached, missing or malformed context must
/// never reach the bridge: only an owned Runtime may receive credentials.
pub fn owned_runtime_context(state: &Value) -> Option<(String, String)> {
    let context = state.get("runtime_context")?;
    if context.get("kind")?.as_str()? != "owned" {
        return None;
    }
    let runtime_id = context.get("runtimeID")?.as_str()?.to_string();
    let storage = context.get("storage")?.as_str()?.to_string();
    Some((runtime_id, storage))
}

/// The Remote Control summary the title bar and connection window use; no
/// secrets. The host/runtime identity is what the private pipe already
/// attested, and `session_ids` is the local publication gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountStatus {
    pub enabled: bool,
    pub connected: bool,
    pub host_id: Option<String>,
    pub runtime_id: Option<String>,
    pub host_public_key: Option<String>,
    pub hub_url: Option<String>,
    pub account_id: Option<String>,
    pub session_ids: Vec<String>,
}

/// A short-lived pairing invitation. Its secret is delivered only in the URI
/// fragment; this type deliberately does not implement `Debug`.
#[derive(Clone)]
pub struct Invitation {
    pub pairing_id: String,
    pub secret: String,
    pub hub_url: String,
    pub host_id: String,
    pub runtime_id: String,
    pub host_public_key: String,
    pub expires_at: i64,
}

impl Invitation {
    /// `miao://pair#<base64url JSON>`, the form the phone scanner and link
    /// entry accept. The secret stays in the fragment.
    pub fn uri(&self) -> String {
        let value = serde_json::json!({
            "version": 1,
            "pairingID": self.pairing_id,
            "secret": self.secret,
            "hubURL": self.hub_url,
            "hostID": self.host_id,
            "runtimeID": self.runtime_id,
            "hostPublicKey": self.host_public_key,
            "expiresAt": self.expires_at,
        });
        format!(
            "miao://pair#{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&value).unwrap_or_default())
        )
    }
}

/// A device that claimed an invitation and waits for the owner's exact-key
/// approval. The policy is what the grant will carry if approved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub pairing_id: String,
    pub public_key: String,
    pub label: String,
    pub permissions: Vec<String>,
    pub project_ids: Vec<String>,
    pub session_ids: Vec<String>,
    pub expires_at: i64,
}

/// An approved device grant. `version` is the observed version a revoke must
/// name, so a stale window cannot revoke a newer grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub version: u64,
    pub public_key: String,
    pub label: String,
    pub permissions: Vec<String>,
    pub project_ids: Vec<String>,
    pub session_ids: Vec<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
}

/// Project a Remote Access status payload; absent fields stay absent rather
/// than failing the whole summary.
pub fn parse_status(data: &Value) -> Option<AccountStatus> {
    Some(AccountStatus {
        enabled: data.get("enabled")?.as_bool()?,
        connected: data.get("connected")?.as_bool()?,
        host_id: text(data, "hostID"),
        runtime_id: text(data, "runtimeID"),
        host_public_key: text(data, "hostPublicKey"),
        hub_url: text(data, "hubURL"),
        account_id: text(data, "accountID"),
        session_ids: strings(data.get("sessionIDs")),
    })
}

/// Parse the invitation `invite` returns. A malformed reply has no usable
/// secret, so it is a whole-reply failure rather than a partial row.
pub fn parse_invitation(data: &Value) -> Option<Invitation> {
    if data.get("version")?.as_u64()? != 1 {
        return None;
    }
    Some(Invitation {
        pairing_id: data.get("pairingID")?.as_str()?.to_string(),
        secret: data.get("secret")?.as_str()?.to_string(),
        hub_url: data.get("hubURL")?.as_str()?.to_string(),
        host_id: data.get("hostID")?.as_str()?.to_string(),
        runtime_id: data.get("runtimeID")?.as_str()?.to_string(),
        host_public_key: data.get("hostPublicKey")?.as_str()?.to_string(),
        expires_at: data.get("expiresAt")?.as_i64()?,
    })
}

/// Parse `pending`. A row missing its pairing identity or candidate key is
/// skipped: one malformed entry must not hide the devices still waiting.
pub fn parse_pending(data: &Value) -> Vec<Candidate> {
    data.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let candidate = row.get("candidate")?;
            Some(Candidate {
                pairing_id: row.get("pairingID")?.as_str()?.to_string(),
                public_key: candidate.get("publicKey")?.as_str()?.to_string(),
                label: candidate
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                permissions: strings(row.pointer("/policy/permissions")),
                project_ids: strings(row.pointer("/policy/projectIDs")),
                session_ids: strings(row.pointer("/policy/sessionIDs")),
                expires_at: row.get("expiresAt")?.as_i64()?,
            })
        })
        .collect()
}

/// Parse `devices`. Revoked grants stay in the reply, so callers can decide
/// whether to show history; the connection window filters them out.
pub fn parse_devices(data: &Value) -> Vec<Grant> {
    data.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some(Grant {
                id: row.get("id")?.as_str()?.to_string(),
                version: row.get("version")?.as_u64()?,
                public_key: row.get("publicKey")?.as_str()?.to_string(),
                label: row
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                permissions: strings(row.get("permissions")),
                project_ids: strings(row.get("projectIDs")),
                session_ids: strings(row.get("sessionIDs")),
                created_at: row.get("createdAt")?.as_i64()?,
                expires_at: row.get("expiresAt")?.as_i64()?,
                revoked_at: row.get("revokedAt").and_then(Value::as_i64),
            })
        })
        .collect()
}

fn text(data: &Value, key: &str) -> Option<String> {
    data.get(key).and_then(Value::as_str).map(str::to_string)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_cannot_retarget_a_bound_request_or_forward_unknown_errors() {
        let request = Request {
            storage: None,
            runtime_id: Some("observed-runtime".into()),
            operation: Operation::Pending,
        };
        assert!(matches!(
            decode(
                br#"{"version":1,"ok":true,"runtimeID":"other-runtime","data":[]}"#,
                &request,
                Error::Unavailable
            ),
            Err(Error::RuntimeChanged)
        ));
        assert!(matches!(
            decode(
                br#"{"version":1,"ok":false,"error":"private-provider-body"}"#,
                &request,
                Error::Unavailable
            ),
            Err(Error::Unconfirmed)
        ));
        assert!(matches!(
            decode(
                br#"{"version":1,"ok":true,"runtimeID":"observed-runtime","credential":"secret"}"#,
                &request,
                Error::Unavailable
            ),
            Err(Error::Unavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn private_pipe_closes_stdin_and_reaps_a_timed_out_writer() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("mtty-control-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("fixture");
        std::fs::write(&executable, "#!/bin/sh\n[ \"$1 $2\" = 'runtime access' ] || exit 1\ncat >/dev/null\nprintf '%s' '{\"version\":1,\"ok\":true,\"runtimeID\":\"observed-runtime\",\"data\":[]}'\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let request = Request {
            storage: None,
            runtime_id: Some("observed-runtime".into()),
            operation: Operation::Pending,
        };
        assert!(call_with_deadline(
            &executable,
            &request,
            &AtomicBool::new(false),
            Duration::from_secs(2)
        )
        .is_ok());
        // This child does not read stdin; an oversized request below the wire
        // limit blocks its pipe writer on platforms with small pipe buffers.
        std::fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let request = Request {
            storage: None,
            runtime_id: Some("observed-runtime".into()),
            operation: Operation::Setup {
                hub_url: "https://relay.example.invalid".into(),
                email: "owner@example.invalid".into(),
                password: "x".repeat(60_000),
                name: "host".into(),
            },
        };
        let started = Instant::now();
        assert!(matches!(
            call_with_deadline(
                &executable,
                &request,
                &AtomicBool::new(false),
                Duration::from_millis(50)
            ),
            Err(Error::Unconfirmed)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn only_owned_pane_context_reaches_the_bridge() {
        let owned = serde_json::json!({
            "runtime_context": { "kind": "owned", "runtimeID": "runtime-1", "storage": "storage.db" }
        });
        assert_eq!(
            owned_runtime_context(&owned),
            Some(("runtime-1".into(), "storage.db".into()))
        );
        let attached = serde_json::json!({ "runtime_context": { "kind": "attached" } });
        assert_eq!(owned_runtime_context(&attached), None);
        assert_eq!(owned_runtime_context(&serde_json::json!({})), None);
        let malformed =
            serde_json::json!({ "runtime_context": { "kind": "owned", "runtimeID": "runtime-1" } });
        assert_eq!(owned_runtime_context(&malformed), None);
    }

    #[test]
    fn an_invitation_round_trips_through_the_scan_uri() {
        let invitation = parse_invitation(&serde_json::json!({
            "version": 1,
            "pairingID": "pair-one",
            "secret": "a".repeat(64),
            "hubURL": "https://hub.example.invalid",
            "hostID": "host-one",
            "runtimeID": "runtime-one",
            "hostPublicKey": "k".repeat(87),
            "expiresAt": 1_800_000_000_000_i64
        }))
        .expect("a complete invitation");
        let uri = invitation.uri();
        let encoded = uri
            .strip_prefix("miao://pair#")
            .expect("native pairing URI");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .expect("base64url payload");
        let decoded: Value = serde_json::from_slice(&bytes).expect("JSON payload");
        assert_eq!(decoded["version"], 1);
        assert_eq!(decoded["pairingID"], "pair-one");
        assert_eq!(decoded["hostID"], "host-one");
        assert_eq!(decoded["runtimeID"], "runtime-one");
        assert_eq!(decoded["expiresAt"], 1_800_000_000_000_i64);
        assert_eq!(decoded["secret"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn pending_skips_rows_without_the_identity_or_candidate_key() {
        let pending = parse_pending(&serde_json::json!([
            {
                "pairingID": "pair-one",
                "candidate": { "publicKey": "key-one", "label": "iPhone", "clientChallenge": "c" },
                "policy": {
                    "permissions": ["read", "prompt"],
                    "projectIDs": [],
                    "sessionIDs": ["ses_1"],
                    "expiresAt": 2_000_000_000_000_i64
                },
                "expiresAt": 1_800_000_000_000_i64
            },
            { "pairingID": "missing-candidate" },
            { "candidate": { "publicKey": "key-two" }, "expiresAt": 1 }
        ]));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].pairing_id, "pair-one");
        assert_eq!(pending[0].public_key, "key-one");
        assert_eq!(pending[0].label, "iPhone");
        assert_eq!(pending[0].permissions, ["read", "prompt"]);
        assert_eq!(pending[0].session_ids, ["ses_1"]);
    }

    #[test]
    fn devices_keep_the_observed_version_and_revocation_state() {
        let devices = parse_devices(&serde_json::json!([
            {
                "id": "grant-one",
                "version": 2,
                "publicKey": "key-one",
                "label": "iPhone",
                "permissions": ["read"],
                "projectIDs": ["prj_1"],
                "sessionIDs": [],
                "createdAt": 1,
                "expiresAt": 2,
                "revokedAt": null
            },
            {
                "id": "grant-two",
                "version": 3,
                "publicKey": "key-two",
                "label": "iPad",
                "permissions": ["prompt"],
                "projectIDs": [],
                "sessionIDs": ["ses_1"],
                "createdAt": 1,
                "expiresAt": 2,
                "revokedAt": 9
            }
        ]));
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].version, 2);
        assert_eq!(devices[0].revoked_at, None);
        assert_eq!(devices[1].revoked_at, Some(9));
        assert_eq!(devices[1].session_ids, ["ses_1"]);
    }

    #[test]
    fn status_summary_keeps_absent_fields_absent() {
        let status = parse_status(&serde_json::json!({
            "enabled": true, "connected": true, "hubURL": "https://hub.example.invalid", "accountID": "acct-1"
        }))
        .unwrap();
        assert!(status.enabled && status.connected);
        assert_eq!(
            status.hub_url.as_deref(),
            Some("https://hub.example.invalid")
        );
        assert_eq!(status.account_id.as_deref(), Some("acct-1"));
        let partial =
            parse_status(&serde_json::json!({ "enabled": false, "connected": false })).unwrap();
        assert_eq!(partial.hub_url, None);
        assert_eq!(partial.account_id, None);
        assert_eq!(parse_status(&serde_json::json!({ "enabled": true })), None);
    }
}
