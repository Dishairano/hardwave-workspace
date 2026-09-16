use futures_util::stream::StreamExt as _;
use crate::models::AuthResponse;
use serde::{Deserialize, Deserializer};
use std::sync::OnceLock;
use std::time::Duration;

/// Deserialize a JSON number or string into a String.
fn id_from_json<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v: serde_json::Value = Deserialize::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::String(s) => Ok(s),
        _ => Err(serde::de::Error::custom("expected number or string")),
    }
}

/// Shared HTTP client with connection pooling and timeouts.
pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(10)
            .build()
            .expect("Failed to create HTTP client")
    })
}

/// API base URLs — override via HW_API_BASE and HW_WS_API_BASE env vars at build time.
// `Option::unwrap_or` is not const-stable, so this cannot run in a const
// initialiser on current toolchains (it compiled on the April 2026 Rust and
// broke afterwards). `match` is const-callable and does the same job.
const BASE_URL: &str = match option_env!("HW_API_BASE") {
    Some(v) => v,
    None => "https://hardwavestudios.com/api",
};
const WS_BASE: &str = match option_env!("HW_WS_API_BASE") {
    Some(v) => v,
    None => "https://workspace.hardwavestudios.com/api",
};

/// Expose WS_BASE for SSE URL construction in other modules.
pub fn ws_base() -> &'static str { WS_BASE }

/// Retry a fallible async operation up to `max_retries` times with exponential backoff.
pub async fn with_retry<T>(
    f: impl Fn() -> futures_util::future::BoxFuture<'static, Result<T, String>>,
    max_retries: u32,
) -> Result<T, String> {
    let mut last_err = String::new();
    for attempt in 0..=max_retries {
        if attempt > 0 {
            let delay = Duration::from_millis(500 * 2u64.pow(attempt - 1));
            tokio::time::sleep(delay).await;
        }
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                last_err = e;
                eprintln!("[API] Retry {}/{} failed: {}", attempt, max_retries, last_err);
            }
        }
    }
    Err(format!("Failed after {} retries: {}", max_retries, last_err))
}

pub async fn login(email: &str, password: &str) -> Result<AuthResponse, String> {
    let res = http_client()
        .post(format!("{}/auth/login", BASE_URL))
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .map_err(|e| format!("Login failed: {}", e))?;

    let body = res.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str::<AuthResponse>(&body)
        .map_err(|e| format!("Parse error: {}", e))
}

pub async fn logout(token: &str) -> Result<(), String> {
    let _ = http_client()
        .post(format!("{}/auth/logout", BASE_URL))
        .bearer_auth(token)
        .send()
        .await;
    Ok(())
}

pub async fn get_auth_status(token: &str) -> Result<bool, String> {
    let res = http_client()
        .get(format!("{}/auth/me", BASE_URL))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(res.status().is_success())
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Workspace {
    #[serde(deserialize_with = "id_from_json")]
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // mirrors the API payload
pub struct WorkspaceFile {
    #[serde(deserialize_with = "id_from_json")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub folder_path: Option<String>,
    pub sha256: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// List all workspaces for the authenticated user.
pub async fn list_workspaces(token: &str) -> Result<Vec<Workspace>, String> {
    let res = http_client()
        .get(format!("{}/workspaces", WS_BASE))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("Failed to list workspaces: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("API error: {}", res.status()));
    }

    let body = res.text().await.map_err(|e| e.to_string())?;
    let data: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

    // Handle both array and { items: [...] } response formats
    let workspaces: Vec<Workspace> = if let Some(arr) = data.as_array() {
        serde_json::from_value(serde_json::Value::Array(arr.clone())).unwrap_or_default()
    } else if let Some(items) = data.get("items").or(data.get("workspaces")) {
        serde_json::from_value(items.clone()).unwrap_or_default()
    } else {
        vec![]
    };

    Ok(workspaces)
}

/// List all files in a workspace.
pub async fn list_files(token: &str, workspace_id: &str) -> Result<Vec<WorkspaceFile>, String> {
    let res = http_client()
        .get(format!("{}/workspaces/{}/files?all=true", WS_BASE, workspace_id))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("Failed to list files: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("API error: {}", res.status()));
    }

    let body = res.text().await.map_err(|e| e.to_string())?;
    let data: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

    let files: Vec<WorkspaceFile> = if let Some(arr) = data.as_array() {
        serde_json::from_value(serde_json::Value::Array(arr.clone())).unwrap_or_default()
    } else if let Some(items) = data.get("items").or(data.get("files")) {
        serde_json::from_value(items.clone()).unwrap_or_default()
    } else {
        vec![]
    };

    Ok(files)
}

/// A folder in a workspace.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // mirrors the API payload; not every field is consumed yet
pub struct WorkspaceFolder {
    #[serde(deserialize_with = "id_from_json")]
    pub id: String,
    pub name: String,
    pub parent_id: Option<serde_json::Value>,
    pub path: Option<String>,
}

/// List all folders in a workspace.
pub async fn list_folders(token: &str, workspace_id: &str) -> Result<Vec<WorkspaceFolder>, String> {
    let res = http_client()
        .get(format!("{}/workspaces/{}/folders", WS_BASE, workspace_id))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("Failed to list folders: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("API error: {}", res.status()));
    }

    let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    let folders: Vec<WorkspaceFolder> = if let Some(items) = data.get("folders") {
        serde_json::from_value(items.clone()).unwrap_or_default()
    } else {
        vec![]
    };

    Ok(folders)
}

/// Get a presigned download URL for a file.
pub async fn get_download_url(token: &str, workspace_id: &str, file_id: &str) -> Result<String, String> {
    let res = http_client()
        .get(format!("{}/workspaces/{}/files/{}", WS_BASE, workspace_id, file_id))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("Failed to get download URL: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("API error: {}", res.status()));
    }

    let data: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    data.get("url")
        .or(data.get("downloadUrl"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "No download URL in response".into())
}

/// Initiate a file upload (get presigned upload URL).
/// The API sends each value twice for compatibility: `uploadUrl` and
/// `upload_url`, `fileId` (a number) and `file_id` (a string). A serde `alias`
/// maps both spellings onto one field, which serde then rejects as a duplicate
/// field — so every upload failed before it began. Read the raw shape and pick
/// whichever spelling is present.
#[derive(Debug, Deserialize)]
struct RawUploadInit {
    #[serde(rename = "uploadUrl", default)]
    upload_url_camel: Option<String>,
    #[serde(rename = "upload_url", default)]
    upload_url_snake: Option<String>,
    #[serde(rename = "fileId", default)]
    file_id_camel: Option<serde_json::Value>,
    #[serde(rename = "file_id", default)]
    file_id_snake: Option<serde_json::Value>,
    /// Set when the server already holds these exact bytes under a ready file.
    #[serde(rename = "alreadyUploaded", default)]
    already_uploaded: Option<bool>,
}

#[derive(Debug)]
pub struct UploadInitResponse {
    pub upload_url: String,
    pub file_id: String,
    pub already_uploaded: bool,
}

impl UploadInitResponse {
    fn from_raw(r: RawUploadInit) -> Result<Self, String> {
        let already_uploaded = r.already_uploaded.unwrap_or(false);
        let upload_url = r
            .upload_url_camel
            .or(r.upload_url_snake)
            .ok_or_else(|| "no uploadUrl in the reply".to_string())?;
        let id = r
            .file_id_camel
            .or(r.file_id_snake)
            .ok_or_else(|| "no fileId in the reply".to_string())?;
        // Sent as a number under one name and a string under the other.
        let file_id = match id {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(v) => v,
            other => return Err(format!("unexpected fileId: {other}")),
        };
        Ok(Self { upload_url, file_id, already_uploaded })
    }
}

pub async fn init_upload(
    token: &str,
    workspace_id: &str,
    filename: &str,
    size: u64,
    folder_path: Option<&str>,
    sha256: &str,
) -> Result<UploadInitResponse, String> {
    let mut body = serde_json::json!({
        "name": filename,
        "size": size,
        "sha256": sha256,
    });
    if let Some(fp) = folder_path {
        body["folder_path"] = serde_json::Value::String(fp.to_string());
    }

    let res = http_client()
        .post(format!("{}/workspaces/{}/files", WS_BASE, workspace_id))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Upload init failed: {}", e))?;

    if !res.status().is_success() {
        let err = res.text().await.unwrap_or_default();
        return Err(format!("Upload init error: {}", err));
    }

    // Read the text first: reqwest's .json() hides the serde detail behind
    // "error decoding response body", which says nothing about which field
    // was wrong.
    let body = res.text().await.map_err(|e| format!("Upload init read failed: {e}"))?;
    let raw = serde_json::from_str::<RawUploadInit>(&body).map_err(|e| {
        format!("Upload init returned an unexpected reply ({e}): {}",
                body.chars().take(200).collect::<String>())
    })?;
    UploadInitResponse::from_raw(raw)
}

/// Confirm upload completion.
pub async fn register_upload(token: &str, workspace_id: &str, file_id: &str) -> Result<(), String> {
    let res = http_client()
        .post(format!("{}/workspaces/{}/files/register", WS_BASE, workspace_id))
        .bearer_auth(token)
        .json(&serde_json::json!({ "file_id": file_id }))
        .send()
        .await
        .map_err(|e| format!("Register failed: {}", e))?;

    if !res.status().is_success() {
        let err = res.text().await.unwrap_or_default();
        return Err(format!("Register error: {}", err));
    }
    Ok(())
}

/// Upload a file to a presigned S3 URL by streaming from disk.
pub async fn upload_to_s3(upload_url: &str, file_path: &std::path::Path) -> Result<(), String> {
    let file = tokio::fs::File::open(file_path).await.map_err(|e| format!("Open file error: {}", e))?;
    let file_size = file.metadata().await.map_err(|e| e.to_string())?.len();
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = reqwest::Body::wrap_stream(stream);

    // Not http_client(): its 60 s is a TOTAL deadline that includes sending the
    // body, so any file that could not upload within a minute failed on every
    // retry, forever. This deadline grows with the file (25 KB/s worst case).
    let deadline = upload_deadline(file_size);
    let res = tokio::time::timeout(
        deadline,
        upload_client()
            .put(upload_url)
            .header("content-length", file_size)
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| format!("S3 upload timed out after {} s", deadline.as_secs()))?
    .map_err(|e| format!("S3 upload failed: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("S3 upload error: {}", res.status()));
    }
    Ok(())
}


// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

/// Files above this go up in parts, so a dropped connection costs one part
/// instead of the whole file.
const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024;
/// S3 requires parts of at least 5 MB (except the last).
const PART_SIZE: u64 = 16 * 1024 * 1024;
/// Parts of one file uploading at the same time, so a single large file can use more of the line.
const PART_PARALLEL: usize = 4;
const PART_ATTEMPTS: u32 = 4;

/// Client for S3 transfers. No total deadline (see upload_to_s3); a
/// connection that stops delivering a response still errors after 120 s.
fn upload_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(10)
            .build()
            .expect("Failed to create upload HTTP client")
    })
}

/// Time allowed for sending `bytes`: a minute, plus the time it takes at
/// 25 KB/s. Generous on purpose; it only has to catch a transfer that hangs.
fn upload_deadline(bytes: u64) -> Duration {
    Duration::from_secs(60 + bytes / 25_000)
}

/// Upload one file and return its remote file id. Returns Ok only once the
/// server has marked the file ready: after `register_upload` for a single
/// PUT, or after the multipart `complete` for a large file.
pub async fn upload_file(
    token: &str,
    workspace_id: &str,
    filename: &str,
    size: u64,
    folder_path: Option<&str>,
    sha256: &str,
    path: &std::path::Path,
) -> Result<String, String> {
    if size <= MULTIPART_THRESHOLD {
        let u = init_upload(token, workspace_id, filename, size, folder_path, sha256).await?;
        // The server matched name, folder, size and SHA-256 against a ready file:
        // those bytes are already stored, so there is nothing to send or register.
        if u.already_uploaded {
            return Ok(u.file_id);
        }
        upload_to_s3(&u.upload_url, path).await?;
        register_upload(token, workspace_id, &u.file_id).await?;
        return Ok(u.file_id);
    }
    upload_multipart(token, workspace_id, filename, size, folder_path, sha256, path).await
}

#[derive(Deserialize)]
struct MultipartInit {
    #[serde(rename = "uploadId")]
    upload_id: String,
    #[serde(rename = "storageKey")]
    storage_key: String,
    #[serde(rename = "fileId", deserialize_with = "id_from_json")]
    file_id: String,
    #[serde(rename = "alreadyUploaded", default)]
    already_uploaded: bool,
}

#[derive(Deserialize)]
struct PartUrls {
    urls: Vec<String>,
}

async fn multipart_call(token: &str, workspace_id: &str, body: &serde_json::Value) -> Result<String, String> {
    let action = body["action"].as_str().unwrap_or("multipart");
    let res = http_client()
        .post(format!("{}/workspaces/{}/files/multipart", WS_BASE, workspace_id))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("Multipart {action} failed: {e}"))?;
    let status = res.status();
    let text = res.text().await.map_err(|e| format!("Multipart {action} read failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("Multipart {action} error {status}: {}", text.chars().take(200).collect::<String>()));
    }
    Ok(text)
}

async fn upload_multipart(
    token: &str,
    workspace_id: &str,
    filename: &str,
    size: u64,
    folder_path: Option<&str>,
    sha256: &str,
    path: &std::path::Path,
) -> Result<String, String> {
    let mut body = serde_json::json!({
        "action": "initiate",
        "filename": filename,
        "sizeBytes": size,
        "sha256": sha256,
        "mimeType": "application/octet-stream",
    });
    if let Some(fp) = folder_path {
        body["folder_path"] = serde_json::Value::String(fp.to_string());
    }
    let text = multipart_call(token, workspace_id, &body).await?;
    let init: MultipartInit = serde_json::from_str(&text).map_err(|e| {
        format!("Multipart initiate returned an unexpected reply ({e}): {}", text.chars().take(200).collect::<String>())
    })?;

    if init.already_uploaded {
        // Identical bytes are already stored. The server still opens a multipart
        // upload for older clients, so close it rather than leave it pending.
        let _ = multipart_call(token, workspace_id, &serde_json::json!({
            "action": "abort", "storageKey": init.storage_key, "uploadId": init.upload_id,
        })).await;
        return Ok(init.file_id);
    }

    let result = send_parts(token, workspace_id, &init, size, path).await;
    if let Err(e) = result {
        // Free the parts already stored and drop the pending row; the caller's
        // retry starts a clean upload.
        let _ = multipart_call(token, workspace_id, &serde_json::json!({
            "action": "abort", "storageKey": init.storage_key, "uploadId": init.upload_id,
        })).await;
        return Err(e);
    }
    Ok(init.file_id)
}

async fn send_parts(
    token: &str,
    workspace_id: &str,
    init: &MultipartInit,
    size: u64,
    path: &std::path::Path,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let part_count = size.div_ceil(PART_SIZE) as u32;

    // Parts go up a few at a time. One at a time meant a single large file was a single
    // connection, so a 4 GB recording could never use more than a slice of the line however much
    // of it was free. Reading stays sequential: the parts are read off disk in order and handed to
    // the uploads as they are read, so memory holds PART_PARALLEL chunks at most.
    let mut file = tokio::fs::File::open(path).await.map_err(|e| format!("Open file error: {e}"))?;
    let mut chunks = Vec::with_capacity(part_count as usize);
    for number in 1..=part_count {
        let offset = u64::from(number - 1) * PART_SIZE;
        let len = PART_SIZE.min(size - offset) as usize;
        let mut chunk = vec![0u8; len];
        file.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| format!("Seek error: {e}"))?;
        file.read_exact(&mut chunk).await.map_err(|e| format!("Read error at part {number}: {e}"))?;
        chunks.push((number, chunk));
    }

    let mut numbered: Vec<(u32, String)> = futures_util::stream::iter(chunks.into_iter().map(
        |(number, chunk)| async move {
            let etag = send_part(token, workspace_id, init, number, chunk).await?;
            Ok::<_, String>((number, etag))
        },
    ))
    .buffer_unordered(PART_PARALLEL)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>, String>>()?;

    // S3 wants the parts listed in order, whatever order they finished in.
    numbered.sort_by_key(|(number, _)| *number);
    let parts: Vec<serde_json::Value> = numbered
        .into_iter()
        .map(|(number, etag)| serde_json::json!({ "PartNumber": number, "ETag": etag }))
        .collect();

    multipart_call(token, workspace_id, &serde_json::json!({
        "action": "complete", "storageKey": init.storage_key, "uploadId": init.upload_id, "parts": parts,
    })).await?;
    Ok(())
}

/// PUT one part. The presigned URL is fetched right before every attempt:
/// they expire after 15 minutes, which a whole large file easily outlasts.
async fn send_part(
    token: &str,
    workspace_id: &str,
    init: &MultipartInit,
    number: u32,
    chunk: Vec<u8>,
) -> Result<String, String> {
    let mut last_err = String::new();
    for attempt in 0..PART_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
        let text = match multipart_call(token, workspace_id, &serde_json::json!({
            "action": "get-part-urls", "storageKey": init.storage_key, "uploadId": init.upload_id,
            "firstPart": number, "partCount": 1,
        })).await {
            Ok(t) => t,
            Err(e) => { last_err = e; continue; }
        };
        let url = match serde_json::from_str::<PartUrls>(&text).ok().and_then(|u| u.urls.into_iter().next()) {
            Some(u) => u,
            None => { last_err = format!("no URL for part {number}"); continue; }
        };

        let deadline = upload_deadline(chunk.len() as u64);
        let sent = tokio::time::timeout(
            deadline,
            upload_client()
                .put(&url)
                .header("content-length", chunk.len())
                .body(chunk.clone())
                .send(),
        )
        .await;
        match sent {
            Ok(Ok(res)) if res.status().is_success() => {
                match res.headers().get("etag").and_then(|v| v.to_str().ok()) {
                    Some(etag) if !etag.is_empty() => return Ok(etag.to_string()),
                    _ => last_err = format!("part {number}: storage returned no ETag"),
                }
            }
            Ok(Ok(res)) => last_err = format!("part {number}: storage error {}", res.status()),
            Ok(Err(e)) => last_err = format!("part {number}: {e}"),
            Err(_) => last_err = format!("part {number}: timed out after {} s", deadline.as_secs()),
        }
        eprintln!("[API] Part {number} attempt {}/{} failed: {last_err}", attempt + 1, PART_ATTEMPTS);
    }
    Err(format!("Upload failed at part {number}: {last_err}"))
}
