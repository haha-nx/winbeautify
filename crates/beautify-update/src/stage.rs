//! Downloading, verifying and unpacking an update payload.
//!
//! [`stage`] turns a [`Release`] into a ready-to-apply payload directory:
//!
//! ```text
//! <staging_root>/0.2.1/
//! ├── update.zip           the downloaded release zip
//! ├── update.zip.sha256    the sidecar it is verified against
//! └── payload/             the unpacked files, ready to copy over the install
//! ```
//!
//! # Why the hash is checked
//!
//! The download rides plain HTTPS to GitHub, which authenticates the
//! transport but not the artifact. The `.sha256` sidecar — produced by the
//! same release script as the zip — is fetched over a *separate request*, so
//! a truncated or corrupted zip is caught by a mismatch rather than by the
//! installer half-applying it. This is integrity, not signing: it defends
//! against bad downloads, not against a compromised release feed. If that
//! ever matters, the sidecar is the place to hang a signature off.
//!
//! # Why extraction is `tar.exe`
//!
//! Windows ships bsdtar in System32 (10 1803+, and this app targets 22H2+),
//! and it unpacks zip archives natively. That is zero extraction code and
//! zero extraction dependencies; bsdtar also refuses `..` components and
//! absolute paths unless asked for them (we never ask), which is the
//! zip-slip story. A dedicated `zip` crate would be several dependencies and
//! a fourth place to get entry-name handling wrong.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::release::Release;

/// Download the release zip and its `.sha256` sidecar, verify the hash, and
/// unpack the zip into a `payload/` directory. Returns that directory.
///
/// Old staging for *other* versions is swept here too: an abandoned
/// `0.1.9/` directory from an update that never got applied would otherwise
/// sit in the user's `%LOCALAPPDATA%` forever.
pub fn stage(release: &Release, staging_root: &Path) -> Result<PathBuf, String> {
    let dir = staging_root.join(&release.version);
    // A previous attempt at this same version may have left a half-written
    // zip or a half-unpacked payload; the cleanest recovery is a fresh dir.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建更新暂存目录: {e}"))?;

    sweep_other_versions(staging_root, &release.version);

    let zip_path = dir.join("update.zip");
    download(&release.zip_url, &zip_path)?;

    let sidecar_path = dir.join("update.zip.sha256");
    download(&release.sha256_url, &sidecar_path)?;

    verify_hash(&zip_path, &sidecar_path)?;

    let payload = dir.join("payload");
    std::fs::create_dir_all(&payload).map_err(|e| format!("无法创建 payload 目录: {e}"))?;
    extract_zip(&zip_path, &payload)?;
    tracing::info!(payload = %payload.display(), "update payload staged");

    Ok(payload)
}

/// Fetch `url` into `dest` (streamed — the release zip is tens of megabytes
/// and must never be held in memory whole).
fn download(url: &str, dest: &Path) -> Result<(), String> {
    let client = crate::release::client()?;
    let mut response = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("下载 {url} 失败: {e}"))?;
    let mut file =
        File::create(dest).map_err(|e| format!("无法写入 {}: {e}", dest.display()))?;
    response
        .copy_to(&mut file)
        .map_err(|e| format!("下载 {url} 中断: {e}"))?;
    Ok(())
}

/// Check the zip against its sidecar. The sidecar's format is whatever
/// `sha256sum`/`Get-FileHash`-style tools emit — `<hex>` or `<hex>  <name>` —
/// so the hash is found as *the* 64-hex-character token in the text.
fn verify_hash(zip: &Path, sidecar: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(sidecar)
        .map_err(|e| format!("无法读取校验文件: {e}"))?;
    let expected = expected_hash(&text)
        .ok_or_else(|| "校验文件里没有找到 SHA-256 值".to_string())?;
    let actual = sha256_hex(zip)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(format!(
            "更新包校验失败（下载不完整或被篡改）：期望 {expected}，实际 {actual}"
        ));
    }
    tracing::info!(zip = %zip.display(), "update zip hash verified");
    Ok(())
}

/// Pull the SHA-256 out of a sidecar text: the first whitespace-delimited
/// token that is exactly 64 hex characters.
fn expected_hash(text: &str) -> Option<String> {
    text.split_whitespace().find(|token| {
        token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())
    }).map(str::to_string)
}

/// SHA-256 of a file, lowercase hex. Streaming: same reason as the download.
pub fn sha256_hex(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("无法打开 {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Unpack the zip with the system's bsdtar. Fails loudly on a non-zero exit:
/// a silently half-extracted payload would fail later, further from the cause.
///
/// `tar.exe` is resolved from the Windows directory rather than `PATH`: the
/// app can inherit a developer's PATH (where a GNU tar that cannot read zip
/// may shadow the system one), and this one location is fixed on every
/// Windows 10 1803+ install.
fn extract_zip(zip: &Path, into: &Path) -> Result<(), String> {
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let tar = Path::new(&windir).join("System32").join("tar.exe");
    let output = std::process::Command::new(&tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(into)
        .output()
        .map_err(|e| format!("无法运行 tar.exe 解压更新包: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("; ");
        return Err(format!("解压更新包失败: {tail}"));
    }
    Ok(())
}

/// Remove staging directories left behind by versions that never got applied.
/// Best effort: anything held open (a file browser sitting in the directory)
/// just survives until the next sweep.
fn sweep_other_versions(staging_root: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(staging_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == keep || crate::version::parse_version(name).is_none() {
            continue;
        }
        tracing::debug!(dir = %entry.path().display(), "sweeping stale update staging");
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_hashes_are_found_in_both_common_formats() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(expected_hash(hash).as_deref(), Some(hash), "bare hash");
        assert_eq!(
            expected_hash(&format!("{hash}  WinBeautify-v0.2.1-x64.zip")).as_deref(),
            Some(hash),
            "sha256sum format"
        );
    }

    #[test]
    fn non_hash_tokens_are_ignored() {
        assert_eq!(expected_hash("no hash in here"), None);
        assert_eq!(expected_hash("too short: ba7816bf"), None);
        // 63 and 65 characters are both wrong; a hash is exactly 64.
        assert_eq!(expected_hash(&"a".repeat(63)), None);
        assert_eq!(expected_hash(&"a".repeat(65)), None);
    }

    #[test]
    fn the_known_sha256_vector_checks_out() {
        // RFC 6234 test vector for the empty input... actually "abc":
        // sha256("abc") is the canonical first vector in FIPS 180-4 examples.
        let dir = std::env::temp_dir().join("winbeautify-sha256-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc.txt");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha256_hex(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn staging_sweeps_other_versions_but_keeps_the_current_one() {
        let root = std::env::temp_dir().join(format!("wb-stage-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("0.1.9")).unwrap();
        std::fs::create_dir_all(root.join("0.2.0")).unwrap();
        std::fs::create_dir_all(root.join("not-a-version")).unwrap();
        std::fs::create_dir_all(root.join("0.3.0")).unwrap();
        sweep_other_versions(&root, "0.3.0");
        assert!(!root.join("0.1.9").exists(), "older staging must be swept");
        assert!(!root.join("0.2.0").exists(), "older staging must be swept");
        assert!(root.join("not-a-version").exists(), "non-version dirs are not ours to delete");
        assert!(root.join("0.3.0").exists(), "the version being staged must survive");
        let _ = std::fs::remove_dir_all(&root);
    }
}
