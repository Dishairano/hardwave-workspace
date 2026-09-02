//! Windows Files On-Demand.
//!
//! Files appear in Explorer at full size but hold no data until something reads
//! them, at which point Windows calls us to fetch the bytes. This is the same
//! mechanism OneDrive uses, via the Cloud Filter API (`cldflt.sys`).
//!
//! Proven on 2026-09-02: `CfRegisterSyncRoot` succeeds from an unpackaged,
//! unsigned binary. Package identity is only needed for the branded entry in
//! Explorer's navigation pane, which we do without.
//!
//! Everything here is a no-op on other platforms so the rest of the sync engine
//! does not need `cfg` guards.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// What the sync engine tells us about a remote file so we can fake it locally.
#[derive(Clone, Debug)]
pub struct RemoteFile {
    /// Path relative to the sync root, using `/` separators.
    pub rel_path: String,
    pub size: u64,
    /// Opaque handle we get back when Windows asks us to fetch the contents.
    /// We store `workspace_id/file_id` so the hydration callback can find it.
    pub identity: String,
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn is_supported() -> bool { false }
    pub fn register(_root: &Path, _provider_id: &str) -> Result<(), String> {
        Err("Files On-Demand is Windows only".into())
    }
    pub fn unregister(_root: &Path) -> Result<(), String> { Ok(()) }
    pub fn create_placeholders(_root: &Path, _dir: &str, _files: &[RemoteFile]) -> Result<u32, String> {
        Err("Files On-Demand is Windows only".into())
    }
    pub fn is_placeholder(_path: &Path) -> bool { false }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::mem::size_of;

    use windows::core::{GUID, HRESULT, PCWSTR};
    use windows::Win32::Storage::CloudFilters::*;

    /// Stable identity for Hardwave across versions, so Windows correlates our
    /// sync roots even if the display name changes.
    const PROVIDER_GUID: u128 = 0x48c1_9b3e_7a41_4f9d_9c22_5c0e_2f7a_1b44;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn err(hr: HRESULT, what: &str) -> String {
        format!("{what} failed: 0x{:08X} — {}", hr.0, hr.message())
    }

    /// Whether this machine has the Cloud Filter API at all. Anything below
    /// Windows 10 1709 does not, and we fall back to normal downloads.
    pub fn is_supported() -> bool {
        unsafe { CfGetPlatformInfo() }.is_ok()
    }

    /// Claim a directory tree as ours. Safe to call repeatedly: the UPDATE flag
    /// re-registers rather than failing on an existing root.
    pub fn register(root: &Path, provider_name: &str) -> Result<(), String> {
        std::fs::create_dir_all(root).map_err(|e| format!("create sync root: {e}"))?;

        let root_w = wide(&root.to_string_lossy());
        let name_w = wide(provider_name);
        let version_w = wide(env!("CARGO_PKG_VERSION"));

        let registration = CF_SYNC_REGISTRATION {
            StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
            ProviderName: PCWSTR(name_w.as_ptr()),
            ProviderVersion: PCWSTR(version_w.as_ptr()),
            SyncRootIdentity: std::ptr::null(),
            SyncRootIdentityLength: 0,
            FileIdentity: std::ptr::null(),
            FileIdentityLength: 0,
            ProviderId: GUID::from_u128(PROVIDER_GUID),
        };

        // FULL hydration rather than PROGRESSIVE: a DAW opening a sample reads
        // the header then seeks, and partial data would make that unpredictable.
        // Audio files are small enough that fetching the whole thing is fine.
        //
        // AUTO_DEHYDRATION_ALLOWED lets Windows reclaim space on its own when
        // the disk fills up, which is the behaviour users already expect.
        let policies = CF_SYNC_POLICIES {
            StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
            Hydration: CF_HYDRATION_POLICY {
                Primary: CF_HYDRATION_POLICY_PRIMARY(CF_HYDRATION_POLICY_FULL.0),
                Modifier: CF_HYDRATION_POLICY_MODIFIER(
                    CF_HYDRATION_POLICY_MODIFIER_AUTO_DEHYDRATION_ALLOWED.0,
                ),
            },
            // ALWAYS_FULL, not FULL. We enumerate the entire remote index and
            // create every placeholder up front, so the namespace really is
            // always present locally.
            //
            // FULL was wrong and made Explorer painfully slow: it tells the
            // platform to ask the provider to enumerate any directory it thinks
            // is incomplete, via a FETCH_PLACEHOLDERS callback we never
            // registered. Explorer asked, nothing answered, and every folder
            // open sat waiting for the timeout. ALWAYS_FULL tells the platform
            // never to forward enumeration at all.
            Population: CF_POPULATION_POLICY {
                Primary: CF_POPULATION_POLICY_PRIMARY(CF_POPULATION_POLICY_ALWAYS_FULL.0),
                Modifier: CF_POPULATION_POLICY_MODIFIER(0),
            },
            InSync: CF_INSYNC_POLICY_TRACK_ALL,
            HardLink: CF_HARDLINK_POLICY_NONE,
            PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
        };

        unsafe {
            CfRegisterSyncRoot(
                PCWSTR(root_w.as_ptr()),
                &registration,
                &policies,
                CF_REGISTER_FLAG_UPDATE | CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT,
            )
        }
        .map_err(|e| err(e.code(), "CfRegisterSyncRoot"))
    }

    /// Hand the directory tree back to Windows. Called on sign-out so we do not
    /// leave a dead provider behind, which makes Explorer misbehave.
    pub fn unregister(root: &Path) -> Result<(), String> {
        let root_w = wide(&root.to_string_lossy());
        unsafe { CfUnregisterSyncRoot(PCWSTR(root_w.as_ptr())) }
            .map_err(|e| err(e.code(), "CfUnregisterSyncRoot"))
    }

    /// Create placeholders for `files` inside `dir` (relative to the sync root).
    ///
    /// Each one costs about 1 KB on disk but reports its true size to Explorer
    /// and to any application that stats it.
    pub fn create_placeholders(
        root: &Path,
        dir: &str,
        files: &[RemoteFile],
    ) -> Result<u32, String> {
        if files.is_empty() {
            return Ok(0);
        }

        let target: PathBuf = if dir.is_empty() { root.to_path_buf() } else { root.join(dir) };
        std::fs::create_dir_all(&target).map_err(|e| format!("create dir: {e}"))?;
        let target_w = wide(&target.to_string_lossy());

        // The Win32 call borrows these buffers, so they have to outlive it.
        let names: Vec<Vec<u16>> = files
            .iter()
            .map(|f| wide(f.rel_path.rsplit('/').next().unwrap_or(&f.rel_path)))
            .collect();
        let idents: Vec<Vec<u16>> = files.iter().map(|f| wide(&f.identity)).collect();

        let mut infos: Vec<CF_PLACEHOLDER_CREATE_INFO> = files
            .iter()
            .enumerate()
            .map(|(i, f)| CF_PLACEHOLDER_CREATE_INFO {
                RelativeFileName: PCWSTR(names[i].as_ptr()),
                FsMetadata: CF_FS_METADATA {
                    FileSize: f.size as i64,
                    BasicInfo: Default::default(),
                },
                FileIdentity: idents[i].as_ptr() as *const c_void,
                FileIdentityLength: (idents[i].len() * 2) as u32,
                // Mark in-sync straight away: we have just learned this file's
                // state from the server, so it is by definition current.
                Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
                Result: HRESULT(0),
                CreateUsn: 0,
            })
            .collect();

        let mut created: u32 = 0;
        let hr = unsafe {
            CfCreatePlaceholders(
                PCWSTR(target_w.as_ptr()),
                &mut infos,
                CF_CREATE_FLAG_NONE,
                Some(&mut created),
            )
        };

        // A batch can partially succeed: the call returns an error while some
        // entries were created. Report per-file failures rather than discarding
        // the whole batch.
        if let Err(e) = hr {
            let failed: Vec<String> = infos
                .iter()
                .enumerate()
                .filter(|(_, i)| i.Result.is_err())
                .map(|(n, i)| format!("{} (0x{:08X})", files[n].rel_path, i.Result.0))
                .collect();
            if created == 0 {
                return Err(err(e.code(), "CfCreatePlaceholders"));
            }
            eprintln!(
                "[CloudFiles] {} of {} placeholders created; failures: {}",
                created,
                files.len(),
                failed.join(", ")
            );
        }
        Ok(created)
    }

    /// Is this path a placeholder we own, rather than a real file? Used to skip
    /// dehydrated files when scanning for local changes — reading one would
    /// hydrate it, which is exactly what we are trying to avoid.
    pub fn is_placeholder(path: &Path) -> bool {
        use windows::Win32::Storage::FileSystem::{
            GetFileAttributesW, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
            FILE_ATTRIBUTE_RECALL_ON_OPEN, INVALID_FILE_ATTRIBUTES,
        };
        let w = wide(&path.to_string_lossy());
        let attrs = unsafe { GetFileAttributesW(PCWSTR(w.as_ptr())) };
        if attrs == INVALID_FILE_ATTRIBUTES {
            return false;
        }
        let mask = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0 | FILE_ATTRIBUTE_RECALL_ON_OPEN.0;
        attrs & mask != 0
    }
}

#[allow(unused_imports)]
pub use imp::{create_placeholders, is_placeholder, is_supported, register, unregister};
