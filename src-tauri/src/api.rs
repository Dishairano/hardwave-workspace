use crate::models::AuthResponse;
use serde::{Deserialize, Deserializer, Serialize};
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
const BASE_URL: &str = option_env!("HW_API_BASE").unwrap_or("https://hardwavestudios.com/api");
const WS_BASE: &str = option_env!("HW_WS_API_BASE").unwrap_or("https://workspace.hardwavestudios.com/api");

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

#[derive(Debug, Deserialize)]
pub struct Workspace {
    #[serde(deserialize_with = "id_from_json")]
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Deserialize)]
pub struct UploadInitResponse {
    #[serde(alias = "uploadUrl")]
    pub upload_url: String,
    #[serde(alias = "fileId")]
    pub file_id: String,
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

    res.json::<UploadInitResponse>()
        .await
        .map_err(|e| format!("Parse error: {}", e))
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

    let res = http_client()
        .put(upload_url)
        .header("content-length", file_size)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("S3 upload failed: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("S3 upload error: {}", res.status()));
    }
    Ok(())
}
