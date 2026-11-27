import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  Account, Bookmark, BookmarkScope, BookmarkSnapshot, Bootstrap, ConnectionLimit, Credentials,
  Destination, EmailSignInPhase, LargePlan, LinkFileInspection, MetadataField, QueueEvent, QueueSnapshot,
  SearchDetail, SearchGenre, SearchId, SearchQuery, SearchReview, SearchSnapshot, SearchSubmission,
  SearchSubmissionReport, Settings, SettingsPatch, SubmissionReport, ValidationResult,
} from "./ipc-types";

export type * from "./ipc-types";

function subscribe<T>(command: string, receive: (snapshot: T) => void) {
  const channel = new Channel<T>(receive);
  return invoke<void>(command, { channel });
}

export const desktop = {
  // Application lifecycle
  bootstrap: () => invoke<Bootstrap>("bootstrap"),
  requestClose: () => invoke<{ requiresConfirmation: boolean }>("request_close"),
  shutdownSession: () => invoke<void>("shutdown_session"),
  openAboutLink: (link: "project" | "qobuz-privacy") => invoke<void>("open_about_link", { link }),

  // Queue & downloads
  queueSnapshot: () => invoke<QueueSnapshot>("queue_snapshot"),
  setQueuePaused: (paused: boolean) => invoke<QueueSnapshot>("set_queue_paused", { paused }),
  subscribeQueue: (receive: (event: QueueEvent) => void) => subscribe("subscribe_queue", receive),
  submitLinks: (text: string, destination: Destination, qualityOverride?: number) =>
    invoke<SubmissionReport>("submit_links", { text, destination, qualityOverride }),
  inspectLinkFile: (path: string) => invoke<LinkFileInspection>("inspect_link_file", { path }),
  submitImportedLinks: (text: string, destination: Destination, qualityOverride?: number) =>
    invoke<SubmissionReport>("submit_imported_links", { text, destination, qualityOverride }),
  retryJob: (jobId: number) => invoke<void>("retry_job", { jobId }),
  retryFile: (jobId: number, fileId: number) => invoke<void>("retry_file", { jobId, fileId }),
  replaceSearchAudio: (jobId: number, fileId: number, token: string) =>
    invoke<void>("replace_search_audio", { jobId, fileId, token }),
  releaseFolderStatus: (jobId: number) =>
    invoke<{ available: boolean; reason: string | null }>("release_folder_status", { jobId }),
  openContainingFolder: (jobId: number, fileId: number) =>
    invoke<void>("open_containing_folder", { jobId, fileId }),
  openReleaseFolder: (jobId: number) => invoke<void>("open_release_folder", { jobId }),
  downloadAgain: (jobId: number) => invoke<void>("download_again", { jobId }),
  cancelJob: (jobId: number) => invoke<void>("cancel_job", { jobId }),
  removeJob: (jobId: number) => invoke<void>("remove_job", { jobId }),
  clearDownloads: () => invoke<{ removed: number; protected: number }>("clear_downloads"),
  applySettingsToPending: (ids: number[]) => invoke<void>("apply_settings_to_pending", { ids }),
  decideLargeSubmission: (plan: LargePlan, decision: boolean) =>
    invoke<void>("decide_large_submission", { id: plan.id, planRevision: plan.planRevision, decision }),
  retryConnection: () => invoke<void>("retry_connection"),

  // Bookmarks
  subscribeBookmarks: (receive: (snapshot: BookmarkSnapshot) => void) => subscribe("subscribe_bookmarks", receive),
  listBookmarks: () => invoke<Bookmark[]>("list_bookmarks"),
  downloadBookmarks: (scope: BookmarkScope) => invoke<SubmissionReport>("download_bookmarks", { scope }),
  retryBookmark: (id: number) => invoke<void>("retry_bookmark", { id }),
  removeBookmarks: (ids: number[]) => invoke<void>("remove_bookmarks", { ids }),

  // Catalog search
  searchGenres: () => invoke<SearchGenre[]>("search_genres"),
  search: (request: SearchQuery) => invoke<SearchSnapshot>("search_query", { request }),
  cancelSearch: (sequence: number) => invoke<void>("search_cancel", { sequence }),
  searchDetail: (request: { sequence: number; generation: number; key: SearchId; cursor?: string | null; retry?: boolean }) =>
    invoke<SearchDetail>("search_detail", { request: { retry: false, ...request } }),
  searchArtwork: (generation: number, key: SearchId) => invoke<string | null>("search_artwork", { generation, key }),
  prepareSearch: (request: SearchSubmission) => invoke<SearchReview>("prepare_search_submission", { request }),
  commitSearch: (token: string) => invoke<SearchSubmissionReport>("commit_search_submission", { token }),
  clearSearchHistory: () => invoke<string>("clear_search_history"),

  // Settings & metadata
  clearBookmarks: () => invoke<void>("clear_bookmarks"),
  readSettings: () => invoke<Settings>("read_settings"),
  patchSettings: (expectedRevision: number, patch: SettingsPatch) =>
    invoke<Settings>("patch_settings", { expectedRevision, patch }),
  saveConnectionLimit: (expectedRevision: number, limit: ConnectionLimit) =>
    invoke<{ settings: Settings; queue: QueueSnapshot }>("save_connection_limit", { expectedRevision, limit }),
  migrateTheme: (expectedRevision: number, legacyDark: boolean) =>
    invoke<Settings>("migrate_theme", { expectedRevision, legacyDark }),
  resetSettings: (expectedRevision: number) => invoke<Settings>("reset_settings", { expectedRevision }),
  metadataCatalog: () => invoke<MetadataField[]>("metadata_catalog"),
  previewOutput: (folder: string, filename: string, quality: number, safeMode?: boolean) =>
    invoke<{ path: string }>("preview_output", { template: { folder, filename }, quality, safeMode }),

  // Account & keyring
  removeCredentials: () => invoke<Account>("reset_vault", { confirmation: true }),
  retryVaultAccess: () => invoke<Account>("retry_vault_access"),
  validateCredentials: (attempt: string, input?: Credentials) =>
    invoke<ValidationResult>("validate_credentials", { attempt, input }),
  startEmailSignIn: (attempt: string, receive: (phase: EmailSignInPhase) => void) =>
    invoke<ValidationResult>("start_email_sign_in", { attempt, phase: new Channel(receive) }),
  confirmAccountValidation: (attempt: string) => invoke<Account>("confirm_account_validation", { attempt }),
  cancelAccountValidation: (attempt: string) => invoke<void>("cancel_account_validation", { attempt }),
};

export function automaticSetupFailure(code?: string | null): boolean {
  return code === "credential_discovery" || code === "invalid_app_id" || code === "account_rejected";
}

export function errorCode(error: unknown): string | undefined {
  if (typeof error === "object" && error !== null && "code" in error && typeof error.code === "string") return error.code;
}

export function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error && typeof error === "object" && "message" in error && typeof error.message === "string") {
    return error.message;
  }
  return "The desktop operation could not be completed.";
}
