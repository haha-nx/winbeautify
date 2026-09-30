//! Applying a staged payload to the install directory.
//!
//! # The rename fallback
//!
//! Windows protects a running exe and a mapped DLL against writes and
//! deletes — but not against a *rename on the same volume*. That asymmetry is
//! the entire trick: a file that cannot be overwritten (`winbeautify.exe`
//! still running as the helper itself, `beautify_taskbar_tap.dll` still mapped
//! by an explorer that refused to die) is renamed to `<name>.old`, which frees
//! the target path, and the new file is copied in. The `.old` copy keeps
//! serving whoever mapped it until they exit, and [`cleanup_old_files`] — run
//! by the app on every startup — removes it once the lock is gone.
//!
//! Worst case, a `.old` file lingers until the shell has restarted at some
//! point in the future. It costs disk space and nothing else, which is why
//! the fallback is unconditional rather than an error: an update that
//! installs 95% of itself and leaves one renamed leftover is a *successful*
//! update as far as the user is concerned.

use std::path::{Path, PathBuf};

/// The file the payload must contain for an apply to even start. A zip
/// without the app binary is a broken release, and applying it would wipe the
/// install directory of everything *except* the one file that matters.
pub const EXE_FILE_NAME: &str = "winbeautify.exe";

/// Every file that a valid payload contains. Used by the release script and
/// by [`cleanup_old_files`]'s callers to name the `.old` leftovers precisely.
pub const PAYLOAD_FILES: &[&str] = &[EXE_FILE_NAME, "beautify_taskbar_tap.dll"];

/// Does `payload` contain the files an apply needs? Call this *before* any
/// file on the install side is touched: a half-valid payload must be rejected
/// while the old install is still intact.
pub fn payload_is_valid(payload: &Path) -> Result<(), String> {
    if !payload.join(EXE_FILE_NAME).is_file() {
        return Err(format!(
            "更新包不完整：缺少 {EXE_FILE_NAME}（发布包格式有误？）"
        ));
    }
    for name in PAYLOAD_FILES {
        if !payload.join(name).is_file() {
            tracing::warn!(file = name, "payload is missing an expected file");
        }
    }
    Ok(())
}

/// Copy the payload over the install directory.
///
/// The exe is copied *last*: if anything in the payload fails to apply, the
/// abort happens with the old (still runnable) exe still in place, and the
/// helper can simply relaunch it rather than leaving the user with a broken
/// install and no way to start the app.
pub fn apply_payload(payload: &Path, install_dir: &Path) -> Result<(), String> {
    payload_is_valid(payload)?;

    let mut files = collect_files(payload)?;
    files.sort_by_key(|(_, rel)| rel == Path::new(EXE_FILE_NAME));

    for (abs, rel) in &files {
        let dest = install_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("无法创建目录 {}: {e}", parent.display()))?;
        }
        copy_with_rename_fallback(abs, &dest)
            .map_err(|e| format!("安装 {rel:?} 失败: {e}"))?;
        tracing::debug!(file = %rel.display(), "applied");
    }
    Ok(())
}

/// Copy `src` to `dest`, moving an in-use `dest` aside first.
///
/// The first attempt is a plain copy. When that fails — the overwhelming
/// majority case being "dest is open or image-mapped" — the existing file is
/// renamed to `<name>.old`, `<name>.old.1`, … until one rename sticks, and
/// the copy is retried onto the now-free path.
fn copy_with_rename_fallback(src: &Path, dest: &Path) -> Result<(), String> {
    if std::fs::copy(src, dest).is_ok() {
        return Ok(());
    }
    if dest.exists() {
        let mut cleared = false;
        for i in 0..10 {
            let aside = aside_path(dest, i);
            if std::fs::rename(dest, &aside).is_ok() {
                tracing::info!(
                    aside = %aside.display(),
                    "old file still in use; renamed aside, the next startup sweeps it"
                );
                cleared = true;
                break;
            }
        }
        if !cleared {
            return Err(format!("{} 正被占用，且无法腾挪", dest.display()));
        }
    }
    std::fs::copy(src, dest).map_err(|e| e.to_string()).map(|_| ())
}

/// `<name>.old` for the first try, `<name>.old.1`… for the retries — a fresh
/// suffix per leftover, because several updates can land between two shell
/// restarts and each one's old file is still mapped until the shell dies.
fn aside_path(dest: &Path, i: usize) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let aside = if i == 0 {
        format!("{name}.old")
    } else {
        format!("{name}.old.{i}")
    };
    dest.with_file_name(aside)
}

/// Remove the `.old` leftovers of previous applies. Called on app startup,
/// when the processes that mapped them (the previous app instance, the old
/// shell) are usually long gone. Explicitly keyed to the known file names —
/// nothing in the user's install directory is deleted on a pattern guess.
pub fn cleanup_old_files(install_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(install_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let is_leftover = PAYLOAD_FILES
            .iter()
            .any(|base| name.strip_prefix(base).is_some_and(|rest| rest.starts_with(".old")));
        if !is_leftover {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => tracing::debug!(file = name, "swept an update leftover"),
            // Still mapped (a shell that never restarted since the update).
            // It will be swept by a later startup; not worth a warning.
            Err(_) => tracing::debug!(file = name, "leftover still locked; left in place"),
        }
    }
}

/// Every file under `payload`, as `(absolute, relative)` pairs, recursing
/// into subdirectories (the payload is flat today; the release format is
/// allowed to grow one).
fn collect_files(payload: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::from("")];
    while let Some(rel) = stack.pop() {
        let abs = payload.join(&rel);
        let meta = std::fs::metadata(&abs)
            .map_err(|e| format!("无法读取更新包内容 {}: {e}", abs.display()))?;
        if meta.is_dir() {
            let entries =
                std::fs::read_dir(&abs).map_err(|e| format!("无法列出 {}: {e}", abs.display()))?;
            for entry in entries.flatten() {
                stack.push(rel.join(entry.file_name()));
            }
        } else if meta.is_file() {
            files.push((abs, rel));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-apply-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The happy path: everything lands, content matches.
    #[test]
    fn a_free_payload_copies_straight_over() {
        let payload = temp_dir("payload");
        let install = temp_dir("install");
        std::fs::write(payload.join(EXE_FILE_NAME), b"new exe").unwrap();
        std::fs::write(payload.join("beautify_taskbar_tap.dll"), b"new dll").unwrap();
        std::fs::write(install.join(EXE_FILE_NAME), b"old exe").unwrap();

        apply_payload(&payload, &install).expect("apply must succeed");

        assert_eq!(std::fs::read(install.join(EXE_FILE_NAME)).unwrap(), b"new exe");
        assert_eq!(
            std::fs::read(install.join("beautify_taskbar_tap.dll")).unwrap(),
            b"new dll"
        );
        assert!(!install.join("winbeautify.exe.old").exists(), "nothing was in use");
    }

    /// The whole point of the fallback: a dest held the way a mapped image
    /// holds its file — reads and deletes shared, *writes* denied — cannot
    /// be copied over, but it *can* be renamed. So the apply succeeds and
    /// leaves a `.old` behind.
    ///
    /// (`share_mode(0)` would simulate a plain exclusive handle, which even
    /// forbids the rename; that is not what a running exe looks like.)
    #[test]
    fn an_in_use_file_is_renamed_aside_and_replaced() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_DELETE: u32 = 0x4;
        let payload = temp_dir("locked-payload");
        let install = temp_dir("locked-install");
        std::fs::write(payload.join(EXE_FILE_NAME), b"new exe").unwrap();
        std::fs::write(install.join(EXE_FILE_NAME), b"old exe").unwrap();

        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(install.join(EXE_FILE_NAME))
            .unwrap();

        apply_payload(&payload, &install).expect("the fallback must make the apply succeed");

        assert_eq!(
            std::fs::read(install.join(EXE_FILE_NAME)).unwrap(),
            b"new exe",
            "the new file must be in place"
        );
        assert_eq!(
            std::fs::read(install.join("winbeautify.exe.old")).unwrap(),
            b"old exe",
            "the in-use old file must have been renamed aside, not destroyed"
        );
        drop(lock);

        // And the sweep removes it once the lock is gone.
        cleanup_old_files(&install);
        assert!(!install.join("winbeautify.exe.old").exists());
    }

    /// Repeated updates between two shell restarts must not collide on the
    /// same `.old` name: each earlier leftover is itself still mapped (that
    /// is why it survived), so the next apply has to take a fresh suffix.
    #[test]
    fn repeated_updates_get_distinct_aside_names() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        let dir = temp_dir("asides");
        let dest = dir.join(EXE_FILE_NAME);
        std::fs::write(&dest, b"v0").unwrap();

        // Two lock flavours, matching what the file system really sees:
        //
        // * the *live* image (dest) shares READ|DELETE — writes denied, so
        //   the copy fails, renames allowed, so the fallback works;
        // * a *leftover* image (the `.old` files) additionally refuses
        //   DELETE — a real mapped image cannot be deleted at all, so a
        //   rename onto an occupied aside name must fail and force the next
        //   suffix.
        let mut live_locks = Vec::new();
        let mut leftover_locks = Vec::new();
        let mut leftover_paths: Vec<PathBuf> = Vec::new();
        for version in 1..=3u8 {
            live_locks.push(
                std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                    .open(&dest)
                    .unwrap(),
            );
            std::fs::write(dir.join(format!("src{version}")), format!("v{version}")).unwrap();
            copy_with_rename_fallback(&dir.join(format!("src{version}")), &dest)
                .expect("each apply must get through the fallback");
            assert_eq!(
                std::fs::read(&dest).unwrap(),
                format!("v{version}").into_bytes(),
                "the new content must be in place"
            );
            // Pin the leftover the way the previous version's running
            // process would still be pinning it.
            let aside = (0..10)
                .map(|i| aside_path(&dest, i))
                .find(|aside| aside.exists() && !leftover_paths.contains(aside))
                .expect("this apply must have created a leftover");
            leftover_paths.push(aside.clone());
            leftover_locks.push(
                std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .open(&aside)
                    .unwrap(),
            );
        }
        drop(live_locks);

        assert_eq!(
            std::fs::read(dest.with_file_name("winbeautify.exe.old")).unwrap(),
            b"v0"
        );
        assert_eq!(
            std::fs::read(dest.with_file_name("winbeautify.exe.old.1")).unwrap(),
            b"v1"
        );
        assert_eq!(
            std::fs::read(dest.with_file_name("winbeautify.exe.old.2")).unwrap(),
            b"v2"
        );

        // One sweep with all the locks gone takes the whole stack out.
        drop(leftover_locks);
        cleanup_old_files(&dir);
        assert!(!dest.with_file_name("winbeautify.exe.old").exists());
        assert!(!dest.with_file_name("winbeautify.exe.old.1").exists());
        assert!(!dest.with_file_name("winbeautify.exe.old.2").exists());
    }

    /// A payload without the exe must be rejected before anything on the
    /// install side is touched.
    #[test]
    fn an_incomplete_payload_is_rejected_up_front() {
        let payload = temp_dir("bad-payload");
        let install = temp_dir("bad-install");
        std::fs::write(install.join(EXE_FILE_NAME), b"old exe").unwrap();
        std::fs::write(payload.join("beautify_taskbar_tap.dll"), b"new dll").unwrap();

        assert!(apply_payload(&payload, &install).is_err());
        assert_eq!(
            std::fs::read(install.join(EXE_FILE_NAME)).unwrap(),
            b"old exe",
            "the install must be untouched"
        );
    }

    /// The cleanup must not delete anything that merely *looks* related —
    /// only `.old` leftovers of the known payload files.
    #[test]
    fn cleanup_only_touches_known_leftovers() {
        let install = temp_dir("cleanup");
        std::fs::write(install.join("winbeautify.exe.old"), b"leftover").unwrap();
        std::fs::write(install.join("beautify_taskbar_tap.dll.old.1"), b"leftover").unwrap();
        std::fs::write(install.join("unrelated.old"), b"keep me").unwrap();
        std::fs::write(install.join("notes.old.bak"), b"keep me").unwrap();

        cleanup_old_files(&install);

        assert!(!install.join("winbeautify.exe.old").exists());
        assert!(!install.join("beautify_taskbar_tap.dll.old.1").exists());
        assert!(install.join("unrelated.old").exists());
        assert!(install.join("notes.old.bak").exists());
    }
}
