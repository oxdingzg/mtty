//! Update manifest parsing, download and SHA-256 verification (ADR 0022).
//!
//! The manifest at `update-check-url` may be either a plain document whose first
//! line is the version, or JSON:
//!
//! ```json
//! { "version": "0.2.0",
//!   "artifacts": { "macos-aarch64": { "url": "https://…", "sha256": "…" } } }
//! ```
//!
//! Downloads run through `curl`; the integrity hash is computed in-crate (a
//! dependency-free SHA-256), so verification never trusts the download path.

use std::collections::BTreeMap;
use std::io::Read;

/// One downloadable artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    /// Optional detached signature (minisign `.sig`) URL.
    pub signature: Option<String>,
}

/// A parsed manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub artifacts: BTreeMap<String, Artifact>,
}

impl Manifest {
    /// The artifact for this platform, if the manifest offers one.
    pub fn for_platform(&self) -> Option<&Artifact> {
        self.artifacts.get(platform_key())
    }
}

/// A stable key for the running platform, e.g. `macos-aarch64`.
pub fn platform_key() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-aarch64",
        ("macos", _) => "macos-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", _) => "linux-x86_64",
        ("windows", _) => "windows-x86_64",
        _ => "unknown",
    }
}

/// Parse a manifest: JSON when it looks like JSON, else first-line version.
pub fn parse(text: &str) -> Manifest {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        if let Some(manifest) = parse_json(trimmed) {
            return manifest;
        }
    }
    Manifest {
        version: text
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('v')
            .to_string(),
        artifacts: BTreeMap::new(),
    }
}

/// Reject malformed responses instead of reporting an invalid version as current.
pub fn parse_checked(text: &str) -> Result<Manifest, &'static str> {
    let manifest = parse(text);
    let parts: Vec<_> = manifest.version.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || part.parse::<u32>().is_err()
        })
    {
        return Err("Invalid update manifest version");
    }
    Ok(manifest)
}

#[derive(serde::Deserialize)]
struct RawManifest {
    version: String,
    #[serde(default)]
    artifacts: BTreeMap<String, RawArtifact>,
}

#[derive(serde::Deserialize)]
struct RawArtifact {
    url: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    signature: Option<String>,
}

fn parse_json(text: &str) -> Option<Manifest> {
    let raw: RawManifest = serde_json::from_str(text).ok()?;
    Some(Manifest {
        version: raw.version.trim_start_matches('v').to_string(),
        artifacts: raw
            .artifacts
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    Artifact {
                        url: v.url,
                        sha256: v.sha256.trim().to_ascii_lowercase(),
                        signature: v.signature,
                    },
                )
            })
            .collect(),
    })
}

/// True when `remote` is a newer version than `local`.
pub fn is_newer(remote: &str, local: &str) -> bool {
    parse_version(remote) > parse_version(local)
}

fn parse_version(s: &str) -> (u32, u32, u32) {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.').map(|p| {
        p.split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("")
            .parse::<u32>()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Hex-encode bytes.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Streaming SHA-256 (FIPS 180-4).
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    length: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: Vec::new(),
            length: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.length += data.len() as u64;
        self.buffer.extend_from_slice(data);
        while self.buffer.len() >= 64 {
            let block: [u8; 64] = self.buffer[..64].try_into().unwrap();
            self.compress(&block);
            self.buffer.drain(..64);
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.length * 8;
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        self.buffer.extend_from_slice(&bit_len.to_be_bytes());
        let blocks: Vec<[u8; 64]> = self
            .buffer
            .chunks(64)
            .map(|c| c.try_into().unwrap())
            .collect();
        for block in blocks {
            self.compress(&block);
        }
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        let add = [a, b, c, d, e, f, g, h];
        for (s, v) in self.state.iter_mut().zip(add) {
            *s = s.wrapping_add(v);
        }
    }
}

/// SHA-256 of a byte slice (used by tests and small buffers).
#[cfg(test)]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize()
}

/// Streaming SHA-256 of a file.
pub fn sha256_file(path: &std::path::Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Whether a file's hex digest matches `expected` (case-insensitive).
pub fn verify_file(path: &std::path::Path, expected: &str) -> std::io::Result<bool> {
    let actual = hex(&sha256_file(path)?);
    Ok(actual.eq_ignore_ascii_case(expected.trim()))
}

/// The release signing key (`minisign.pub` at the repository root); the
/// `update-pubkey` setting replaces it.
pub const RELEASE_PUBKEY: &str = include_str!("../../../minisign.pub");

/// Check a minisign signature over a file without reading it all into memory.
/// `pubkey` is a whole `minisign.pub` file or just its base64 line. When the
/// signature's trusted comment names a file, it must be `file_name`, so a
/// genuine signature cannot vouch for a different artifact.
pub fn verify_signature(
    path: &std::path::Path,
    signature: &str,
    pubkey: &str,
    file_name: &str,
) -> Result<(), String> {
    use minisign_verify::{PublicKey, Signature};
    let key = if pubkey.trim_start().starts_with("untrusted comment:") {
        PublicKey::decode(pubkey.trim())
    } else {
        PublicKey::from_base64(pubkey.trim())
    }
    .map_err(|e| format!("bad update public key: {e}"))?;
    let sig = Signature::decode(signature.trim()).map_err(|e| format!("bad signature: {e}"))?;
    let mut verifier = key
        .verify_stream(&sig)
        .map_err(|e| format!("signature not usable: {e}"))?;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        verifier.update(&buf[..n]);
    }
    verifier
        .finalize()
        .map_err(|_| "the signature does not match the download".to_string())?;
    let named = sig
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("file:"));
    match named {
        Some(named) if named != file_name => {
            Err(format!("the signature is for {named}, not {file_name}"))
        }
        _ => Ok(()),
    }
}

/// The file name at the end of a URL (query and fragment dropped).
pub fn url_file_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let name = path.rsplit('/').next()?;
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['\\', ':'])
        && name.chars().all(|c| !c.is_control());
    ok.then(|| name.to_string())
}

/// dl.mtty.dev serves the same release paths as github.com/mttydev/mtty for
/// networks that cannot reach GitHub, so the mirror is tried first and GitHub
/// is the fallback. A URL outside that pair — an operator-pinned feed, a
/// host we do not mirror — is honored alone, never routed through a stranger.
pub fn mirror_candidates(url: &str) -> Vec<String> {
    const GITHUB: &str = "https://github.com/mttydev/mtty/";
    const MIRROR: &str = "https://dl.mtty.dev/mtty/";
    match url.strip_prefix(GITHUB) {
        Some(rest) => vec![format!("{MIRROR}{rest}"), url.to_string()],
        None => vec![url.to_string()],
    }
}

/// Download an artifact into `dir` and check it before anything uses it: the
/// manifest's SHA-256 and a minisign signature, which is required. A file that
/// fails either check is deleted. Blocks: run it off the UI thread.
pub fn download_verified(
    artifact: &Artifact,
    pubkey: &str,
    dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let sig_url = artifact
        .signature
        .as_deref()
        .ok_or("the update is not signed, so it will not be installed")?;
    let name = url_file_name(&artifact.url).ok_or("the update URL has no file name")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(&name);
    let curl = |args: &[&str]| {
        mtty_platform::background_command("curl")
            .args(["-fsSL", "--proto", "=https,file"])
            .args(args)
            .output()
            .map_err(|e| format!("curl: {e}"))
    };
    // The artifact and its signature live under the same release path, so a
    // mirror rewrite pairs them position for position.
    let artifact_urls = mirror_candidates(&artifact.url);
    let sig_urls = mirror_candidates(sig_url);
    let mut last_error = "the update URL has no mirror candidates".to_string();
    for (url, sig_url) in artifact_urls.iter().zip(sig_urls.iter()) {
        let out = curl(&["--max-time", "900", "-o", &path.to_string_lossy(), url]);
        let out = match out {
            Ok(out) => out,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        if !out.status.success() {
            let _ = std::fs::remove_file(&path);
            last_error = format!(
                "download failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            continue;
        }
        let checked = (|| {
            if !artifact.sha256.trim().is_empty()
                && !verify_file(&path, &artifact.sha256).map_err(|e| e.to_string())?
            {
                return Err("the checksum does not match".to_string());
            }
            let sig = curl(&["--max-time", "60", sig_url])?;
            if !sig.status.success() {
                return Err("could not fetch the signature".to_string());
            }
            verify_signature(&path, &String::from_utf8_lossy(&sig.stdout), pubkey, &name)
        })();
        match checked {
            Ok(()) => return Ok(path),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                last_error = e;
            }
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway key pair made with minisign 0.12 for these tests only.
    const TEST_PUBKEY: &str = "untrusted comment: minisign public key 4D488B93977FBD3F
RWQ/vX+Xk4tITeKYOzgjMfBzFifPoGVTPI7xbE6k2IfMqeqVaOwi1qR8
";
    const TEST_CONTENT: &str = "mtty update test artifact\n";
    const TEST_SIG: &str = "untrusted comment: signature from minisign secret key
RUQ/vX+Xk4tITQUxpMgHQ6UzBw+zqfSjBhLKh2THDXW8/OjUwGWoMkkiUhJcGhGDI5fBdZxResztwM6MCwX7y6vlcATOG1gqhA4=
trusted comment: timestamp:1790849763\tfile:mtty-test.zip\thashed
t+gWHfmlkGM5SoMqXUkBvKwxms2sV0uR79Q4Xsnk42jdyFJ1AS1RuLQKXe0dIrQgW0flxW8/q6jndgI30/2wDQ==
";

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mtty-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn signatures_are_checked_in_process() {
        let dir = scratch("sig");
        let file = dir.join("mtty-test.zip");
        std::fs::write(&file, TEST_CONTENT).unwrap();
        verify_signature(&file, TEST_SIG, TEST_PUBKEY, "mtty-test.zip").unwrap();
        // Just the base64 line works too.
        let b64 = TEST_PUBKEY.lines().nth(1).unwrap();
        verify_signature(&file, TEST_SIG, b64, "mtty-test.zip").unwrap();
        // A genuine signature for another file name is refused.
        let e = verify_signature(&file, TEST_SIG, TEST_PUBKEY, "mtty-other.zip").unwrap_err();
        assert!(e.contains("mtty-test.zip"), "{e}");
        // So is another key, and a changed file.
        assert!(verify_signature(&file, TEST_SIG, RELEASE_PUBKEY, "mtty-test.zip").is_err());
        std::fs::write(&file, "mtty update test artifact!\n").unwrap();
        let e = verify_signature(&file, TEST_SIG, TEST_PUBKEY, "mtty-test.zip").unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        assert!(verify_signature(&file, "garbage", TEST_PUBKEY, "mtty-test.zip").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_release_key_is_the_repository_key() {
        assert!(RELEASE_PUBKEY.contains("RWS6BkKxSQSAwHVm241oVIBam75S9RLwSDV9+k2T0s5mKtgPuXDWJHYm"));
        assert!(minisign_verify::PublicKey::decode(RELEASE_PUBKEY.trim()).is_ok());
    }

    #[test]
    fn downloads_require_checksum_and_signature() {
        let src = scratch("src");
        std::fs::write(src.join("mtty-test.zip"), TEST_CONTENT).unwrap();
        std::fs::write(src.join("mtty-test.zip.sig"), TEST_SIG).unwrap();
        let url = |name: &str| format!("file://{}/{name}", src.display());
        let good = Artifact {
            url: url("mtty-test.zip"),
            sha256: hex(&sha256(TEST_CONTENT.as_bytes())),
            signature: Some(url("mtty-test.zip.sig")),
        };
        let out = scratch("out");
        let path = download_verified(&good, TEST_PUBKEY, &out).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEST_CONTENT);
        let unsigned = Artifact {
            signature: None,
            ..good.clone()
        };
        assert!(download_verified(&unsigned, TEST_PUBKEY, &out)
            .unwrap_err()
            .contains("not signed"));
        std::fs::remove_file(&path).unwrap();
        let bad_sum = Artifact {
            sha256: "00".repeat(32),
            ..good.clone()
        };
        assert!(download_verified(&bad_sum, TEST_PUBKEY, &out).is_err());
        assert!(!path.exists(), "a failed download is deleted");
        assert!(download_verified(&good, RELEASE_PUBKEY, &out).is_err());
        assert!(!path.exists());
        assert_eq!(
            url_file_name("https://x/a/mtty.zip?x=1").as_deref(),
            Some("mtty.zip")
        );
        assert_eq!(url_file_name("https://x/a/"), None);
        std::fs::remove_dir_all(src).unwrap();
        std::fs::remove_dir_all(out).unwrap();
    }

    /// The published release end to end: `MTTY_UPDATE_TEST_REAL=1` downloads
    /// the manifest and this platform's artifact and checks it against the
    /// repository key.
    #[test]
    fn the_published_release_verifies() {
        if std::env::var_os("MTTY_UPDATE_TEST_REAL").is_none() {
            return;
        }
        let url = "https://github.com/mttydev/mtty/releases/latest/download/latest.json";
        let out = mtty_platform::background_command("curl")
            .args(["-fsSL", "--connect-timeout", "10", "--max-time", "60", url])
            .output()
            .unwrap();
        let manifest = parse_checked(&String::from_utf8_lossy(&out.stdout)).unwrap();
        let artifact = manifest
            .for_platform()
            .or_else(|| manifest.artifacts.values().next())
            .unwrap();
        let dir = scratch("real");
        let path = download_verified(artifact, RELEASE_PUBKEY, &dir).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 1_000_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_streaming_matches_oneshot() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut hasher = Sha256::new();
        for chunk in data.chunks(7) {
            hasher.update(chunk);
        }
        assert_eq!(hasher.finalize(), sha256(&data));
    }

    #[test]
    fn parses_plain_and_json_manifests() {
        let plain = parse("v0.2.0\nhttps://example.com\n");
        assert_eq!(plain.version, "0.2.0");
        assert!(plain.artifacts.is_empty());

        let json = parse(
            r#"{ "version": "v0.3.0",
                 "artifacts": { "macos-aarch64": { "url": "https://x/a.dmg", "sha256": "AB",
                                                  "signature": "https://x/a.dmg.sig" } } }"#,
        );
        assert_eq!(json.version, "0.3.0");
        let artifact = &json.artifacts["macos-aarch64"];
        assert_eq!(artifact.url, "https://x/a.dmg");
        assert_eq!(artifact.sha256, "ab");
        assert_eq!(artifact.signature.as_deref(), Some("https://x/a.dmg.sig"));
    }

    #[test]
    fn mirror_candidates_put_the_mirror_first_for_release_urls() {
        let candidates = mirror_candidates(
            "https://github.com/mttydev/mtty/releases/download/v0.3.5/mtty-macos-arm64.zip",
        );
        assert_eq!(
            candidates,
            vec![
                "https://dl.mtty.dev/mtty/releases/download/v0.3.5/mtty-macos-arm64.zip"
                    .to_string(),
                "https://github.com/mttydev/mtty/releases/download/v0.3.5/mtty-macos-arm64.zip"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn mirror_candidates_honor_a_pinned_feed_alone() {
        assert_eq!(
            mirror_candidates("https://example.com/feeds/latest.json"),
            vec!["https://example.com/feeds/latest.json".to_string()]
        );
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn update_check_rejects_failed_or_malformed_responses() {
        for response in [
            "",
            "Not Found",
            "<html>error</html>",
            "{}",
            "{\"version\":\"oops\"}",
            "0.0",
            "0.0.2garbage",
        ] {
            assert!(parse_checked(response).is_err(), "{response}");
        }
        let manifest = parse_checked(r#"{"version":"0.0.3","artifacts":{"macos-aarch64":{"url":"https://example.com/mtty.zip","sha256":"ab"}}}"#).unwrap();
        assert!(is_newer(&manifest.version, "0.0.2"));
        assert_eq!(
            manifest.artifacts["macos-aarch64"].url,
            "https://example.com/mtty.zip"
        );
    }

    #[test]
    fn sha256_matches_the_system_tool() {
        // Cross-check our implementation against shasum/sha256sum when present.
        let path = std::env::temp_dir().join(format!("mtty-xcheck-{}", std::process::id()));
        let data: Vec<u8> = (0..70_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let tool = ["shasum", "sha256sum"].into_iter().find(|t| {
            std::process::Command::new(t)
                .arg("--version")
                .output()
                .is_ok()
                || std::process::Command::new(t).output().is_ok()
        });
        if let Some(tool) = tool {
            let args: Vec<&str> = if tool == "shasum" {
                vec!["-a", "256"]
            } else {
                vec![]
            };
            if let Ok(out) = std::process::Command::new(tool)
                .args(args)
                .arg(&path)
                .output()
            {
                let expected = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if expected.len() == 64 {
                    assert_eq!(
                        hex(&sha256_file(&path).unwrap()),
                        expected,
                        "{tool} mismatch"
                    );
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn verifies_a_file() {
        let path = std::env::temp_dir().join(format!("mtty-digest-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert!(verify_file(
            &path,
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
        )
        .unwrap());
        assert!(!verify_file(&path, "00").unwrap());
        let _ = std::fs::remove_file(&path);
    }
}
