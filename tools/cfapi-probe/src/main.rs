//! Cloud Filter API feasibility probe.
//!
//! Registers a sync root in a temp folder, creates one placeholder, reports what
//! Windows said, then cleans up. Run it, look at the folder in Explorer, and we
//! know whether Files On-Demand is reachable without MSIX packaging.

#![cfg(windows)]

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;

use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Storage::CloudFilters::*;

/// Rust string to a NUL-terminated UTF-16 buffer we keep alive ourselves.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn describe(hr: HRESULT) -> String {
    if hr.is_ok() {
        "OK".to_string()
    } else {
        format!("0x{:08X} — {}", hr.0, hr.message())
    }
}

fn main() {
    println!("Hardwave — Cloud Filter API probe");
    println!("=================================\n");

    // 1. Platform support. Anything below Windows 10 1709 has no cfapi at all.
    let mut info = CF_PLATFORM_INFO::default();
    let hr = unsafe { CfGetPlatformInfo(&mut info) };
    match hr {
        Ok(()) => println!(
            "platform     : build {}.{} (integration 0x{:X})",
            info.BuildNumber, info.RevisionNumber, info.IntegrationNumber
        ),
        Err(e) => {
            println!("platform     : FAILED {}", describe(e.code()));
            println!("\nCloud Filter API is unavailable on this machine. Stop here.");
            return;
        }
    }

    // 2. A scratch sync root under the user's profile.
    let root: PathBuf = dirs_home().join("HardwaveProbe");
    if let Err(e) = std::fs::create_dir_all(&root) {
        println!("sync root    : could not create {} ({e})", root.display());
        return;
    }
    println!("sync root    : {}", root.display());

    let root_w = wide(&root.to_string_lossy());
    let provider = wide("Hardwave Workspace (probe)");
    let version = wide("0.1.0");

    // 3. Register. This is the question: does it work with no package identity?
    let registration = CF_SYNC_REGISTRATION {
        StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(provider.as_ptr()),
        ProviderVersion: PCWSTR(version.as_ptr()),
        SyncRootIdentity: std::ptr::null(),
        SyncRootIdentityLength: 0,
        FileIdentity: std::ptr::null(),
        FileIdentityLength: 0,
        ProviderId: windows::core::GUID::from_u128(0x48c1_9b3e_7a41_4f9d_9c22_5c0e2f7a1b44),
    };

    // FULL hydration suits audio: a DAW reading a WAV header then seeking gets
    // the whole file rather than a partial stream. AUTO_DEHYDRATION lets Windows
    // reclaim space on its own.
    let policies = CF_SYNC_POLICIES {
        StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_PRIMARY(CF_HYDRATION_POLICY_FULL.0 as u16),
            Modifier: CF_HYDRATION_POLICY_MODIFIER(
                CF_HYDRATION_POLICY_MODIFIER_AUTO_DEHYDRATION_ALLOWED.0 as u16,
            ),
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_PRIMARY(CF_POPULATION_POLICY_FULL.0 as u16),
            Modifier: CF_POPULATION_POLICY_MODIFIER(0),
        },
        InSync: CF_INSYNC_POLICY_TRACK_ALL,
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };

    let hr = unsafe {
        CfRegisterSyncRoot(
            PCWSTR(root_w.as_ptr()),
            &registration,
            &policies,
            CF_REGISTER_FLAG_UPDATE,
        )
    };
    match &hr {
        Ok(()) => println!("register     : OK  <-- unpackaged registration WORKS"),
        Err(e) => {
            println!("register     : FAILED {}", describe(e.code()));
            println!("\nIf this is ERROR_CLOUD_FILE_ACCESS_DENIED the path permissions are wrong.");
            println!("Any other failure most likely means package identity IS required.");
            return;
        }
    }

    // 4. Create one placeholder: a 5 MB file that occupies almost nothing.
    let name = wide("placeholder-test.wav");
    let ident = wide("probe/placeholder-test.wav");
    let mut created: Vec<CF_PLACEHOLDER_CREATE_INFO> = vec![CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: PCWSTR(name.as_ptr()),
        FsMetadata: CF_FS_METADATA {
            FileSize: 5 * 1024 * 1024,
            BasicInfo: Default::default(),
        },
        FileIdentity: ident.as_ptr() as *const c_void,
        FileIdentityLength: (ident.len() * 2) as u32,
        Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
        Result: windows::core::HRESULT(0),
        CreateUsn: 0,
    }];

    let mut done: u32 = 0;
    let hr = unsafe {
        CfCreatePlaceholders(
            PCWSTR(root_w.as_ptr()),
            &mut created,
            CF_CREATE_FLAG_NONE,
            Some(&mut done),
        )
    };
    match &hr {
        Ok(()) => {
            println!("placeholder  : OK ({done} created)");
            println!("               per-file result: {}", describe(created[0].Result));
        }
        Err(e) => println!("placeholder  : FAILED {}", describe(e.code())),
    }

    // 5. What the filesystem actually reports.
    let file = root.join("placeholder-test.wav");
    match std::fs::metadata(&file) {
        Ok(m) => println!("\nreported size: {} bytes (should read as 5 MB)", m.len()),
        Err(e) => println!("\nreported size: could not stat ({e})"),
    }

    println!("\nOpen this folder in Explorer and check the status column:");
    println!("  {}", root.display());
    println!("A cloud/outline icon and near-zero disk usage means it works.\n");
    println!("Press Enter to unregister and clean up...");
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);

    let hr = unsafe { CfUnregisterSyncRoot(PCWSTR(root_w.as_ptr())) };
    println!("unregister   : {}", match &hr { Ok(()) => "OK".into(), Err(e) => describe(e.code()) });
    let _ = std::fs::remove_dir_all(&root);
    println!("cleaned up.");
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
