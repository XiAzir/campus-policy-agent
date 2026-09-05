export interface CatalogDoc {
  doc_uid: string;
  title: string;
  doc_type: string;
  department: string;
  effective_date: string | null;
  audience: string[];
  domains: Record<string, string[]>;
  line_count: number;
  published_at: string;
  deactivated_kind: "" | "superseded" | "manual";
  notes: string;
}

export interface Citation {
  evidence_id: string;
  doc_uid: string;
  doc_hash: string;
  title: string;
  line_start: number;
  line_end: number;
  page: number | null;
  section: string | null;
  quote: string[];
}

export interface StoredMessage {
  role: "user" | "model";
  text: string;
  citations: Citation[];
  interrupted?: boolean;
  ts: number;
}

export interface ChatRecord {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  messages: StoredMessage[];
}

export interface UserPrefs {
  college: string;
  entryYear: string;
  scopeMode: "auto" | "domains" | "files";
  domains: string[];
  docUids: string[];
  yearMode: "current" | "past";
}

export type ChatEvent =
  | { event: "queued"; position: number; request_id: string }
  | { event: "started"; request_id: string }
  | { event: "retrieving" }
  | { event: "generating" }
  | { event: "delta"; text: string }
  | { event: "citations"; citations: Citation[] }
  | { event: "done"; text: string; interrupted: boolean }
  | { event: "expand_request"; reason: string }
  | { event: "error"; message: string };

export interface SourceText {
  doc_uid: string;
  title: string;
  doc_type: string;
  line_start: number;
  line_end: number;
  line_count: number;
  page: number | null;
  section: string | null;
  deactivated_kind: string;
  lines: string[];
}

export interface AdminPackageDoc {
  doc_hash: string;
  original_filename: string;
  doc_type: string;
  title: string;
  department: string;
  effective_date: string | null;
  audience: string[];
  notes: string;
  replaces_doc_uid: string | null;
  replaces_unresolved: boolean;
  domains: { tag: string; section_ids: string[] }[];
  sections: { section_id: string; title: string; start_line: number; end_line: number }[];
  line_count: number;
  chunk_count: number;
  edited: boolean;
}

export interface AdminPackage {
  id: number;
  sha256: string;
  original_filename: string;
  size: number;
  imported_at: string;
  status: string;
  preprocessing_version: string;
  embed_model: string;
  embed_dim: number;
  doc_count: number;
  chunk_count: number;
  vector_shape?: number[];
  documents: AdminPackageDoc[];
}

export interface AdminStatus {
  disk: { total_gb: number; used_gb: number; free_gb: number; warn_gb: number };
  counts: { documents: number; current: number; chunks: number; packages: number };
  data_dir_mb: number;
}

export interface VersionInfo {
  doc_uid: string;
  title: string;
  published_at: string;
  effective_date: string | null;
  deactivated_kind: string;
  is_current: boolean;
  package_id: number;
}
