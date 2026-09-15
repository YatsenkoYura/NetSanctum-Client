import { invoke } from "@tauri-apps/api/core";

export type ConnectionStatus =
  | "connected"
  | "disconnected"
  | "vault_locked"
  | "unreachable"
  | "credentials_invalid";

export interface ConnectionState {
  status: ConnectionStatus;
  node_url: string | null;
  node_name: string | null;
  message: string | null;
}

export interface SavedPackage {
  node_origin: string;
  package_id: string;
  package_title: string;
  root_url: string;
  status: string;
  byte_size: number;
  saved_at: string;
}

export interface SavedModule {
  node_origin: string;
  module_id: string;
  module_title: string;
  module_root_url: string;
  packages: SavedPackage[];
}

export interface BootstrapState {
  connection: ConnectionState;
  modules: SavedModule[];
  vault_exists: boolean;
}

export interface ConnectRequest {
  node_url: string;
  master_token: string;
  vault_password: string;
  allow_insecure_http: boolean;
  remember_without_password: boolean;
}

export interface ModuleShortcutTarget {
  nodeOrigin: string;
  moduleId: string;
  moduleTitle: string;
  modulePath: string;
}

export function bootstrap(): Promise<BootstrapState> {
  return invoke("bootstrap");
}

export function diagnoseNode(nodeUrl: string, allowInsecureHttp: boolean): Promise<string[]> {
  return invoke("diagnose_node", { nodeUrl, allowInsecureHttp });
}

export function shortcutOnlineAvailable(nodeOrigin: string): Promise<boolean> {
  return invoke("shortcut_online_available", { nodeOrigin });
}

export function downloadPackage(manifestUrl: string): Promise<boolean> {
  return invoke("download_package", { manifestUrl });
}

export function connectNode(request: ConnectRequest): Promise<ConnectionState> {
  return invoke("connect_node", { request });
}

export function saveMobileSession(
  request: ConnectRequest,
  accessToken: string,
  expiresIn: number,
): Promise<ConnectionState> {
  return invoke("save_mobile_session", { request, accessToken, expiresIn });
}

export function disconnectNode(): Promise<ConnectionState> {
  return invoke("disconnect_node");
}

export function resetConnection(): Promise<ConnectionState> {
  return invoke("reset_connection");
}

export function unlockVault(password: string): Promise<ConnectionState> {
  return invoke("unlock_vault", { password });
}

export function unlockShortcut(password: string): Promise<ConnectionState> {
  return invoke("unlock_shortcut", { password });
}

export function openNode(): Promise<void> {
  return invoke("open_node");
}

export function openNodeModule(modulePath: string): Promise<void> {
  return invoke("open_node_module", { modulePath });
}

export function isMobile(): Promise<boolean> {
  return invoke("is_mobile");
}

export function createModuleShortcut(
  nodeOrigin: string,
  moduleId: string,
  moduleTitle: string,
  modulePath: string,
  name: string,
  iconText: string,
): Promise<void> {
  return invoke("create_module_shortcut", {
    nodeOrigin,
    moduleId,
    moduleTitle,
    modulePath,
    name,
    iconText,
  });
}

export function takeMobileShortcut(): Promise<ModuleShortcutTarget | null> {
  return invoke("take_mobile_shortcut");
}

export function listLibrary(): Promise<SavedModule[]> {
  return invoke("list_library");
}

export function deletePackage(nodeOrigin: string, packageId: string): Promise<SavedModule[]> {
  return invoke("delete_package", { nodeOrigin, packageId });
}

export function openOffline(nodeOrigin: string, packageId: string): Promise<void> {
  return invoke("open_offline", { nodeOrigin, packageId });
}

export function openOfflineModule(nodeOrigin: string, moduleId: string): Promise<void> {
  return invoke("open_offline_module", { nodeOrigin, moduleId });
}

export function showHome(): Promise<void> {
  return invoke("show_home");
}

export function navigateBack(): Promise<void> {
  return invoke("navigate_back");
}

export function hideToTray(): Promise<void> {
  return invoke("hide_to_tray");
}
