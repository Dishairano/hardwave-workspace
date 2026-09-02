//! Serving file contents when Windows asks for them.
//!
//! Once a directory is a sync root full of placeholders, opening one triggers a
//! `FETCH_DATA` callback into this process. We stream the bytes from object
//! storage and hand them back in chunks; Windows shows the progress UI and
//! unblocks the reading application when the data lands.
//!
//! This runs on a Windows thread pool, not on Tokio, so anything async has to
//! be handed to the runtime explicitly.

#![allow(dead_code)]

use std::path::Path;

/// Resolves a file identity (what we stored on the placeholder) to bytes.
/// The sync engine supplies this so this module stays free of API details.
pub type Fetcher = std::sync::Arc<
    dyn Fn(String, u64, u64) -> futures_util::future::BoxFuture<'static, Result<Vec<u8>, String>>
        + Send
        + Sync,
>;

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub struct Connection;
    pub fn connect(_root: &Path, _f: Fetcher) -> Result<Connection, String> {
        Err("Files On-Demand is Windows only".into())
    }
    pub fn disconnect(_c: Connection) {}
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::sync::OnceLock;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::NTSTATUS;
    use windows::Win32::Storage::CloudFilters::*;

    /// Windows hands callbacks a raw context pointer, but wiring a per-root
    /// pointer through the C ABI safely is fiddly and we only ever have one
    /// sync root. A process-wide fetcher keeps the unsafe surface small.
    static FETCHER: OnceLock<Fetcher> = OnceLock::new();

    /// Chunk size for feeding data back. Large enough to keep throughput up,
    /// small enough that progress moves visibly on a big sample pack.
    const CHUNK: u64 = 1024 * 1024;

    pub struct Connection {
        key: CF_CONNECTION_KEY,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Windows calls this when something reads a dehydrated placeholder.
    unsafe extern "system" fn on_fetch_data(
        info: *const CF_CALLBACK_INFO,
        params: *const CF_CALLBACK_PARAMETERS,
    ) {
        if info.is_null() || params.is_null() {
            return;
        }
        let info = &*info;
        let params = &*params;

        // The identity blob we attached at placeholder creation: "wsId/fileId".
        let identity = {
            let ptr = info.FileIdentity as *const u16;
            let len = (info.FileIdentityLength / 2) as usize;
            if ptr.is_null() || len == 0 {
                String::new()
            } else {
                let slice = std::slice::from_raw_parts(ptr, len);
                String::from_utf16_lossy(slice).trim_end_matches('\0').to_string()
            }
        };

        let required = params.Anonymous.FetchData;
        let offset = required.RequiredFileOffset as u64;
        // Honour the optional range when it is larger: Windows uses it to hint
        // that we may as well send more while we are here.
        let length = {
            let req = required.RequiredLength as u64;
            let opt = required.OptionalLength as u64;
            req.max(opt).max(1)
        };

        let key = info.ConnectionKey;
        let txn = info.TransferKey;

        let Some(fetch) = FETCHER.get().cloned() else {
            fail_transfer(key, txn, offset, length);
            return;
        };

        // The callback thread must return promptly, so the actual work goes to
        // the Tokio runtime and reports back through CfExecute.
        let handle = tokio::runtime::Handle::try_current();
        let fut = fetch(identity, offset, length);
        match handle {
            Ok(h) => {
                h.spawn(async move {
                    match fut.await {
                        Ok(bytes) => transfer(key, txn, offset, &bytes),
                        Err(e) => {
                            eprintln!("[Hydration] fetch failed: {e}");
                            fail_transfer(key, txn, offset, length);
                        }
                    }
                });
            }
            Err(_) => {
                eprintln!("[Hydration] no Tokio runtime on callback thread");
                fail_transfer(key, txn, offset, length);
            }
        }
    }

    /// Feed bytes back to the platform in chunks.
    fn transfer(key: CF_CONNECTION_KEY, txn: i64, offset: u64, data: &[u8]) {
        let mut sent: u64 = 0;
        let total = data.len() as u64;
        while sent < total {
            let n = CHUNK.min(total - sent);
            unsafe {
                // Zero and assign rather than naming the generated union
                // variant: those names are positional and move between
                // windows-rs releases.
                let mut params: CF_OPERATION_PARAMETERS = std::mem::zeroed();
                params.ParamSize = size_of::<CF_OPERATION_PARAMETERS>() as u32;
                params.Anonymous.TransferData.CompletionStatus = NTSTATUS(0); // STATUS_SUCCESS
                params.Anonymous.TransferData.Buffer = data.as_ptr().add(sent as usize) as *const c_void;
                params.Anonymous.TransferData.Offset = (offset + sent) as i64;
                params.Anonymous.TransferData.Length = n as i64;

                let mut op: CF_OPERATION_INFO = std::mem::zeroed();
                op.StructSize = size_of::<CF_OPERATION_INFO>() as u32;
                op.Type = CF_OPERATION_TYPE_TRANSFER_DATA;
                op.ConnectionKey = key;
                op.TransferKey = txn;

                if let Err(e) = CfExecute(&op, &mut params) {
                    eprintln!("[Hydration] CfExecute failed at offset {}: {:?}", offset + sent, e);
                    return;
                }
            }
            sent += n;
        }
    }

    /// Tell the platform we could not produce the data, so the reading app gets
    /// a clean error instead of hanging.
    fn fail_transfer(key: CF_CONNECTION_KEY, txn: i64, offset: u64, length: u64) {
        unsafe {
            let mut params: CF_OPERATION_PARAMETERS = std::mem::zeroed();
            params.ParamSize = size_of::<CF_OPERATION_PARAMETERS>() as u32;
            // STATUS_UNSUCCESSFUL
            params.Anonymous.TransferData.CompletionStatus = NTSTATUS(0xC000_0001u32 as i32);
            params.Anonymous.TransferData.Buffer = std::ptr::null();
            params.Anonymous.TransferData.Offset = offset as i64;
            params.Anonymous.TransferData.Length = length as i64;

            let mut op: CF_OPERATION_INFO = std::mem::zeroed();
            op.StructSize = size_of::<CF_OPERATION_INFO>() as u32;
            op.Type = CF_OPERATION_TYPE_TRANSFER_DATA;
            op.ConnectionKey = key;
            op.TransferKey = txn;
            let _ = CfExecute(&op, &mut params);
        }
    }

    /// Start serving hydration requests for a sync root.
    pub fn connect(root: &Path, fetcher: Fetcher) -> Result<Connection, String> {
        let _ = FETCHER.set(fetcher);

        let callbacks = [
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_FETCH_DATA,
                Callback: Some(on_fetch_data),
            },
            // Terminator entry: the platform reads until it sees this.
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NONE,
                Callback: None,
            },
        ];

        let root_w = wide(&root.to_string_lossy());
        let key = unsafe {
            CfConnectSyncRoot(
                PCWSTR(root_w.as_ptr()),
                callbacks.as_ptr(),
                None,
                CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
            )
        }
        .map_err(|e| format!("CfConnectSyncRoot failed: 0x{:08X} — {}", e.code().0, e.message()))?;

        Ok(Connection { key })
    }

    pub fn disconnect(c: Connection) {
        let _ = unsafe { CfDisconnectSyncRoot(c.key) };
    }
}

pub use imp::{connect, disconnect, Connection};
