//! Share copies its link without a WebView2 prompt.
//!
//! The page creates the share link with a fetch and only then writes it to the
//! clipboard. By then the click's user activation has expired, so WebView2
//! files the write as a clipboard permission request and shows "wants to see
//! text and images copied to the clipboard" (Windows test machine, 0.2.21,
//! 2026-10-16). We answer that request ourselves, for our own site only, and
//! leave every other permission and origin to WebView2's default.

/// The one origin whose clipboard requests the app grants.
#[cfg(any(windows, test))]
const WORKSPACE_HOST: &str = "workspace.hardwavestudios.com";

/// Whether a permission request's URI is our own site: https, exactly our
/// host, default port. Parsed, not prefix-matched, so a lookalike host or our
/// address inside a query string does not pass.
#[cfg(any(windows, test))]
fn is_workspace_origin(uri: &str) -> bool {
    let Ok(url) = tauri::Url::parse(uri) else {
        return false;
    };
    url.scheme() == "https" && url.host_str() == Some(WORKSPACE_HOST) && url.port().is_none()
}

/// Grant clipboard requests from our site in this window's WebView2.
#[cfg(windows)]
pub fn allow_for_workspace(win: &tauri::WebviewWindow) {
    let res = win.with_webview(|webview| {
        if let Err(e) = add_handler(&webview.controller()) {
            log::warn!("[Clipboard] could not watch permission requests: {e}");
        }
    });
    if let Err(e) = res {
        log::warn!("[Clipboard] {e}");
    }
}

#[cfg(windows)]
fn add_handler(
    controller: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
) -> windows_core::Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ,
        COREWEBVIEW2_PERMISSION_STATE_ALLOW,
    };
    use webview2_com::{take_pwstr, PermissionRequestedEventHandler};

    let handler = PermissionRequestedEventHandler::create(Box::new(|_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
        // SAFETY: `args` is a live COM object WebView2 handed to this callback,
        // and `kind` is a valid place for it to write the permission kind.
        unsafe { args.PermissionKind(&mut kind)? };
        // A clipboard write without user activation is filed as CLIPBOARD_READ.
        if kind != COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ {
            return Ok(());
        }
        let mut uri = windows_core::PWSTR::null();
        // SAFETY: as above; WebView2 allocates the string with CoTaskMemAlloc
        // and take_pwstr copies it and frees it exactly once.
        let uri = unsafe {
            args.Uri(&mut uri)?;
            take_pwstr(uri)
        };
        if is_workspace_origin(&uri) {
            // SAFETY: `args` is still the live object from this callback.
            unsafe { args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)? };
        }
        Ok(())
    }));
    let mut token = 0i64;
    // SAFETY: we are on the window's thread (with_webview runs there), the
    // controller is alive, and `token` is a valid place for the registration
    // token. The handler lives as long as the webview; we never remove it.
    unsafe {
        let core = controller.CoreWebView2()?;
        core.add_PermissionRequested(&handler, &mut token)
    }
}

#[cfg(test)]
mod tests {
    use super::is_workspace_origin;

    #[test]
    fn our_site_is_allowed() {
        assert!(is_workspace_origin(
            "https://workspace.hardwavestudios.com/x"
        ));
        assert!(is_workspace_origin(
            "https://workspace.hardwavestudios.com/"
        ));
        assert!(is_workspace_origin(
            "https://workspace.hardwavestudios.com:443/projects"
        ));
    }

    #[test]
    fn other_origins_are_refused() {
        for uri in [
            "http://workspace.hardwavestudios.com",
            "https://workspace.hardwavestudios.com.evil.com",
            "https://evil.com/?u=https://workspace.hardwavestudios.com",
            "https://evil.com/https://workspace.hardwavestudios.com",
            "https://workspace.hardwavestudios.com@evil.com/",
            "https://workspace.hardwavestudios.com:8443/",
            "https://hardwavestudios.com/",
            "https://sub.workspace.hardwavestudios.com/",
            "workspace.hardwavestudios.com",
            "",
        ] {
            assert!(!is_workspace_origin(uri), "{uri} should be refused");
        }
    }
}
