use crate::models::AuthResponse;
use serde::{Deserialize, Serialize};

const BASE_URL: &str = "https://hardwavestudios.com/api";
const WS_BASE: &str = "https://workspace.hardwavestudios.com/api";

pub async fn login(email: &str, password: &str) -> Result<AuthResponse, String> {
    let client = reqwest::Client::new();
    let res = client
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
    let client = reqwest::Client::new();
    let _ = client
        .post(format!("{}/auth/logout", BASE_URL))
        .bearer_auth(token)
        .send()
        .await;
    Ok(())
}

pub async fn get_auth_status(token: &str) -> Result<bool, String> {
    let client = reqwest::Client::new();
    let res = client
        .get(format!("{}/auth/me", BASE_URL))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(res.status().is_success())
}

#[derive(Debug, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceFile {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub folder_path: Option<String>,
    pub sha256: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize)]
struct ListResponse<T> {
    #[serde(default)]
    items: Vec<T>,
}

/// List all workspaces for the authenticated user.
pub async fn list_workspaces(token: &str) -> Result<Vec<Workspace>, String> {
    let client = reqwest::Client::new();
    let res = client
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
    let client = reqwest::Client::new();
    let res = client
        .get(format!("{}/workspaces/{}/files", WS_BASE, workspace_id))
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

/// Get a presigned download URL for a file.
pub async fn get_download_url(token: &str, workspace_id: &str, file_id: &str) -> Result<String, String> {
    let client = reqwest::Client::new();
    let res = client
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
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "No download URL in response".into())
}

/// Initiate a file upload (get presigned upload URL).
#[derive(Debug, Deserialize)]
pub struct UploadInitResponse {
    pub upload_url: String,
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
    let client = reqwest::Client::new();
    let mut body = serde_json::json!({
        "name": filename,
        "size": size,
        "sha256": sha256,
    });
    if let Some(fp) = folder_path {
        body["folder_path"] = serde_json::Value::String(fp.to_string());
    }

    let res = client
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
    let client = reqwest::Client::new();
    let res = client
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

/// Upload file bytes to a presigned S3 URL.
pub async fn upload_to_s3(upload_url: &str, data: Vec<u8>) -> Result<(), String> {
    let client = reqwest::Client::new();
    let res = client
        .put(upload_url)
        .body(data)
        .send()
        .await
        .map_err(|e| format!("S3 upload failed: {}", e))?;

    if !res.status().is_success() {
        return Err(format!("S3 upload error: {}", res.status()));
    }
    Ok(())
}
