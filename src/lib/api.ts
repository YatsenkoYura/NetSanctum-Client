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
}

export function bootstrap(): Promise<BootstrapState> {
  return invoke("bootstrap");
}

export function connectNode(request: ConnectRequest): Promise<ConnectionState> {
  return invoke("connect_node", { request });
}

export function disconnectNode(): Promise<ConnectionState> {
  return invoke("disconnect_node");
}

export function unlockVault(password: string): Promise<ConnectionState> {
  return invoke("unlock_vault", { password });
}

export function openNode(): Promise<void> {
  return invoke("open_node");
}

export function listLibrary(): Promise<SavedModule[]> {
  return invoke("list_library");
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
