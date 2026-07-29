/** ADE WebSocket message shapes used by the host UI (mirrors fresh-gui-protocol). */

export const PROTOCOL_VERSION = "0.4.0";

export type FsKind = "file" | "dir" | "symlink" | "other";

export interface FsEntry {
  name: string;
  path: string;
  kind: FsKind;
  size?: number;
}

export interface PtyInfo {
  id: string;
  cols: number;
  rows: number;
}

export type ClientMessage =
  | {
      type: "hello";
      protocol_version: string;
      role: "client";
      implementation: string;
      capabilities: string[];
    }
  | { type: "auth"; token: string }
  | { type: "session_create"; layout?: string }
  | { type: "session_attach"; session_id: string }
  | { type: "layout_set"; layout: string }
  | { type: "pty_open"; cols: number; rows: number }
  | { type: "pty_data"; id: string; data: string }
  | { type: "pty_resize"; id: string; cols: number; rows: number }
  | { type: "pty_close"; id: string }
  | { type: "fs_list"; request_id: string; path: string }
  | { type: "fs_watch"; request_id: string; path: string; recursive: boolean }
  | { type: "fs_unwatch"; watch_id: string }
  | { type: "editor_open"; request_id: string; path: string; preview: boolean }
  | { type: "editor_close"; buffer_id: string }
  | {
      type: "buffer_edit";
      request_id: string;
      buffer_id: string;
      base_rev: number;
      text: string;
    }
  | {
      type: "buffer_save";
      request_id: string;
      buffer_id: string;
      base_rev: number;
    };

export type ServerMessage =
  | {
      type: "hello";
      protocol_version: string;
      role: string;
      implementation: string;
      capabilities: string[];
    }
  | { type: "auth_ok" }
  | { type: "auth_error"; message: string }
  | { type: "session_created"; session_id: string }
  | {
      type: "session_attached";
      session_id: string;
      ptys: PtyInfo[];
      layout?: string;
    }
  | { type: "pty_opened"; id: string; cols: number; rows: number }
  | { type: "pty_data"; id: string; data: string }
  | { type: "pty_closed"; id: string; reason?: string }
  | {
      type: "fs_listed";
      request_id: string;
      path: string;
      entries: FsEntry[];
    }
  | {
      type: "editor_opened";
      request_id: string;
      buffer_id: string;
      path: string;
      language?: string;
    }
  | {
      type: "buffer_snapshot";
      buffer_id: string;
      rev: number;
      text: string;
      path: string;
    }
  | { type: "buffer_changed"; request_id: string; buffer_id: string; rev: number }
  | {
      type: "buffer_saved";
      request_id: string;
      buffer_id: string;
      path: string;
      rev: number;
    }
  | { type: "fs_watch_started"; request_id: string; watch_id: string; path: string }
  | { type: "fs_changed"; watch_id: string; paths: string[] }
  | { type: "error"; code: string; message: string };
