import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";

export interface LatestVersion {
  version: string;
  notes: string;
  pub_date?: string;
}

export interface InstallProgress {
  phase:
    | "downloading"
    | "extracting"
    | "shortcuts"
    | "registering"
    | "done";
  percent: number;
  message: string;
}

export interface InstallOptions {
  install_dir: string;
  create_desktop_shortcut: boolean;
  create_start_menu_shortcut: boolean;
  launch_after: boolean;
}

export async function fetchLatestVersion(): Promise<LatestVersion> {
  return invoke<LatestVersion>("fetch_latest_version");
}

export async function defaultInstallDir(): Promise<string> {
  return invoke<string>("default_install_dir");
}

export async function startInstall(options: InstallOptions): Promise<string> {
  return invoke<string>("start_install", { options });
}

export async function cancelInstall(): Promise<void> {
  return invoke("cancel_install");
}

export async function launchInstalled(exePath: string): Promise<void> {
  return invoke("launch_installed", { exePath });
}

export async function quit(): Promise<void> {
  return invoke("quit");
}

export async function onProgress(
  handler: (p: InstallProgress) => void
): Promise<UnlistenFn> {
  return listen<InstallProgress>("install://progress", (e) => handler(e.payload));
}

// ─── Uninstall mode ──────────────────────────────────────────────────────────

export type LaunchMode = "install" | "uninstall";

export type UninstallStep =
  | "wait_for_uploads"
  | "close_app"
  | "delete_local_copies"
  | "remove_install_folder"
  | "remove_shortcuts"
  | "remove_registry_keys";

/** The steps of this uninstall, how many are done, and what a long step is doing. */
export interface UninstallProgress {
  steps: UninstallStep[];
  done: number;
  detail: string | null;
}

/** Why the uninstall stopped: the step that could not finish, what to do, and how many files are not uploaded yet. */
export interface UninstallFailure {
  step: UninstallStep | null;
  reason: string;
  still_uploading: number | null;
}

/** Which screens to show, decided by the exe from its arguments and its name. */
export async function launchMode(): Promise<LaunchMode> {
  return invoke<LaunchMode>("launch_mode");
}

/** The Workspace folder on this PC, for the option on the first screen. Reads only. */
export async function uninstallOverview(): Promise<string> {
  return invoke<string>("uninstall_overview");
}

/** Removes the app, and the downloaded copies in the Workspace folder when `deleteLocalCopies` is set. */
export async function startUninstall(deleteLocalCopies: boolean): Promise<void> {
  try {
    await invoke("start_uninstall", { deleteLocalCopies });
  } catch (e) {
    const failure: UninstallFailure =
      e && typeof e === "object" && "reason" in e
        ? (e as UninstallFailure)
        : { step: null, reason: String(e), still_uploading: null };
    throw failure;
  }
}

export async function onUninstallProgress(
  handler: (p: UninstallProgress) => void
): Promise<UnlistenFn> {
  return listen<UninstallProgress>("uninstall://progress", (e) => handler(e.payload));
}
