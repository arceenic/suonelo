export type Mode = "system" | "light" | "dark";
export type DownloadMode = "sequential" | "concurrent";
export type ConnectionLimit = "auto" | "unlimited" | 1 | 2 | 4 | 8 | 16 | 32;
export type Theme = "Suonelo" | "Vercel" | "Spotify" | "Material Design" | "Claymorphism" | "Violet";

export interface Settings {
  revision: number;
  quality: number;
  intervalSeconds: number;
  maxConnections: ConnectionLimit;
  downloadMode: DownloadMode;
  limitedToAlbumReleases: boolean;
  allowFallback: boolean;
  bookletOnly: boolean;
  createPlaylist: boolean;
  smartDiscography: boolean;
  safeMode: boolean;
  folderTemplate: string;
  filenameTemplate: string;
  lyricsMode: "off" | "embed" | "external";
  artwork: "original" | "1200" | "600" | "300";
  downloadCoverArt: boolean;
  embedCoverArt: boolean;
  coverFilename: string;
  tags: Record<string, boolean>;
  outputPath: string;
  appearance: Mode;
  theme: Theme | null;
  lightTheme?: Theme | null;
  darkTheme?: Theme | null;
  outputMissing: boolean;
}

export type SettingsPatch = Partial<Omit<Settings, "revision" | "outputMissing" | "lightTheme" | "darkTheme" | "theme">> & { theme?: Theme };
export type FileStatus = "queued" | "downloading" | "retrying" | "completed" | "failed" | "cancelled" | "unavailable";
export type FilePhase = "queued" | "connecting" | "transferring" | "processing" | "retrying" | "completed" | "failed" | "cancelled" | "unavailable";

export interface FileItem {
  id: number;
  kind: "audio" | "artwork" | "booklet" | "lyrics" | "playlist";
  title: string;
  album?: string | null;
  disc?: number | null;
  status: FileStatus;
  generation: number;
  attempt: number;
  progress: number | null;
  phase: FilePhase;
  receivedBytes: number;
  totalBytes: number | null;
  finalSizeBytes: number | null;
  quality?: string | null;
  path?: string | null;
  reason?: string | null;
  conflictPath?: string | null;
  replacementToken?: string | null;
  reused?: boolean;
  independentRetry?: boolean;
}

export interface Job {
  id: number;
  url: string;
  title: string;
  subtitle: string;
  status: FileStatus | "incomplete";
  mode?: "audio" | "booklet";
  generation: number;
  progress: number | null;
  clearable: boolean;
  thumbnail?: string | null;
  resourceKind?: string;
  driveBlocked?: boolean;
  canRetry?: boolean;
  reason?: string | null;
  files: FileItem[];
}

export interface LargePlan {
  id: number;
  planRevision: number;
  trackCount: number;
  unit?: "tracks" | "pdfs";
}

export interface QueueSnapshot {
  revision: number;
  jobs: Job[];
  confirmations: LargePlan[];
  canClear: boolean;
  protectedCount: number;
  bytesPerSecond: number;
  effectiveMode: DownloadMode;
  requestedMode: DownloadMode;
  liveLimit: ConnectionLimit | null;
  paused: boolean;
  canPause: boolean;
  recovery?: {
    cooldownSeconds?: number | null;
    retryConnection?: boolean;
    workerCap?: number | null;
  };
}

export interface QueueProgress {
  revision: number;
  jobId: number;
  fileId: number;
  generation: number;
  attempt: number;
  progress: number | null;
  phase: FilePhase;
  receivedBytes: number;
  totalBytes: number | null;
  finalSizeBytes: number | null;
  jobProgress: number | null;
}

export interface QueueSpeed { revision: number; bytesPerSecond: number }
export type QueueEvent = QueueSnapshot | QueueProgress | QueueSpeed;

export interface Bookmark {
  id: number;
  url: string;
  title: string;
  albumArtist?: string | null;
  albumName?: string | null;
  bitDepth?: number | null;
  samplingRate?: number | null;
  reason?: string | null;
  thumbnail?: string | null;
  resourceKind?: string;
}

export interface BookmarkSnapshot {
  revision: number;
  bookmarks: Bookmark[];
}

export interface Account {
  state: "missing" | "validating" | "unvalidated" | "ready" | "rejected";
  epoch?: number;
  hasToken: boolean;
  hasAppId: boolean;
  hasSecret: boolean;
  hasSavedCredentials?: boolean;
  hasRememberedAppPair?: boolean;
  canRememberSavedAppPair?: boolean;
  accessMode?: "unknown" | "full" | "preview";
  reasonCode?: string | null;
  reason?: string | null;
}

export interface Credentials {
  token: string;
  appId?: string;
  secret?: string;
  forceDiscovery?: boolean;
  rememberSavedAppPair?: boolean;
}

export interface MetadataField {
  id: string;
  label: string;
  defaultEnabled: boolean;
  eligible?: boolean;
  description?: string;
  group?: "album" | "track" | null;
  rawPath?: string | null;
  exportTag?: string | null;
}

export interface Bootstrap {
  settings: Settings;
  queue: QueueSnapshot;
  bookmarks: Bookmark[];
  account: Account;
}

export type Destination = "downloads" | "bookmarks";
export type SearchKind = "album" | "track" | "artist";
export type SearchSort = "relevance" | "name_asc" | "name_desc" | "date";
export type SearchAction = "start" | "filter" | "sort" | "more" | "retry";

export interface SearchId {
  kind: SearchKind;
  id: string;
}

export interface SearchGenre {
  id: number;
  label: string;
  parent: number | null;
  path: number[];
  storefrontPath?: number[];
}

export interface SearchRow {
  key: SearchId;
  title: string;
  credits: { id: string; name: string }[];
  byline: string | null;
  edition: string | null;
  date: string | null;
  genreId: number | null;
  bitDepth: number | null;
  sampleRate: number | null;
  hires: boolean | null;
  explicit: boolean | null;
  streamable: boolean | null;
  trackCount: number | null;
  disc: number | null;
  number: number | null;
  duration: number | null;
  release: string | null;
}

export interface SearchGroup {
  rows: SearchRow[];
  count: number;
  cursor: string | null;
  providerTotal: number | null;
  limited: boolean;
  error: { code: string; message: string; retryable: boolean } | null;
}

export interface SearchSnapshot {
  generation: number;
  groups: Record<SearchKind, SearchGroup>;
  history: number;
  historyError: SearchGroup["error"];
}

export interface SearchDetail {
  row: SearchRow;
  group: SearchGroup;
}

export interface SearchQuery {
  sequence: number;
  generation: number;
  query: string;
  genres: number[];
  sort: SearchSort;
  kind: SearchKind;
  action: SearchAction;
  cursor?: string | null;
}

export interface SearchSubmission {
  generation: number;
  category: SearchKind;
  detail?: SearchId;
  wholeRelease: boolean;
  selected: SearchId[];
  destination: Destination;
}

export interface SearchReview {
  token: string | null;
  count: number;
  skipped: SearchId[];
}

export interface SearchSubmissionReport {
  accepted: SearchId[];
  skipped: SearchId[];
  conflicts: SearchId[];
}

export type BookmarkScope = { kind: "all" } | { kind: "selected"; ids: number[] } | { kind: "single"; id: number };

export interface SubmissionReport {
  accepted: string[];
  duplicates: string[];
  invalid: { url: string; reason: string }[];
}

export interface LinkFileInspection {
  valid: boolean;
  filename: string;
  sizeBytes: number;
  encoding: "UTF-8";
  content: string | null;
  totalUrls: number;
  uniqueUrls: number;
  duplicates: number;
  invalidCount: number;
  invalid: { line: number; url: string; reason: string }[];
  reason?: string | null;
}

export type ValidationResult = { account: Account | null; confirmation: "switch" | "active" | null };
export type EmailSignInPhase = "waiting" | "validating";
