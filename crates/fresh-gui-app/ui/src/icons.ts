/** Lightweight SVG explorer icons (VS Code / Terax feel, no icon-pack dependency). */

export type FileIconKind = "folder" | "folder-open" | "symlink" | "file";

export type FileIcon = {
  kind: FileIconKind;
  /** Extra class for file-type tinting, e.g. `icon-rs`. */
  tone?: string;
};

const EXT_TONE: Record<string, string> = {
  rs: "icon-rs",
  ts: "icon-ts",
  tsx: "icon-ts",
  js: "icon-js",
  jsx: "icon-js",
  mjs: "icon-js",
  cjs: "icon-js",
  py: "icon-py",
  md: "icon-md",
  markdown: "icon-md",
  json: "icon-json",
  jsonc: "icon-json",
  toml: "icon-toml",
  yaml: "icon-yaml",
  yml: "icon-yaml",
  css: "icon-css",
  scss: "icon-css",
  html: "icon-html",
  svg: "icon-svg",
  sh: "icon-shell",
  bash: "icon-shell",
  zsh: "icon-shell",
  go: "icon-go",
  c: "icon-c",
  h: "icon-c",
  cpp: "icon-cpp",
  cc: "icon-cpp",
  cxx: "icon-cpp",
  hpp: "icon-cpp",
  java: "icon-java",
  kt: "icon-java",
  rb: "icon-rb",
  php: "icon-php",
  sql: "icon-sql",
  lock: "icon-lock",
  txt: "icon-text",
  log: "icon-text",
  dockerfile: "icon-docker",
};

const SPECIAL_NAME_TONE: Record<string, string> = {
  dockerfile: "icon-docker",
  "docker-compose.yml": "icon-docker",
  "docker-compose.yaml": "icon-docker",
  makefile: "icon-shell",
  "cargo.toml": "icon-toml",
  "package.json": "icon-json",
  "tsconfig.json": "icon-ts",
  "readme.md": "icon-md",
};

export function fileIcon(name: string, kind: string, expanded = false): FileIcon {
  if (kind === "dir") return { kind: expanded ? "folder-open" : "folder", tone: "icon-folder" };
  if (kind === "symlink") return { kind: "symlink", tone: "icon-symlink" };
  const lower = name.toLowerCase();
  const special = SPECIAL_NAME_TONE[lower];
  if (special) return { kind: "file", tone: special };
  const base = lower.includes(".") ? lower.split(".").pop()! : "";
  return { kind: "file", tone: (base && EXT_TONE[base]) || "icon-file" };
}

/** Trusted SVG markup for the explorer icon cell. */
export function iconSvg(icon: FileIcon): string {
  switch (icon.kind) {
    case "folder":
      return `<svg class="vtree-icon-svg" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M1.5 3.5A1.5 1.5 0 0 1 3 2h3.586a1.5 1.5 0 0 1 1.06.44L8.5 3.5H13A1.5 1.5 0 0 1 14.5 5v7A1.5 1.5 0 0 1 13 13.5H3A1.5 1.5 0 0 1 1.5 12V3.5Z"/></svg>`;
    case "folder-open":
      return `<svg class="vtree-icon-svg" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M1.5 4A1.5 1.5 0 0 1 3 2.5h3.086a1 1 0 0 1 .707.293L8.5 4.5H13A1.5 1.5 0 0 1 14.5 6v.5l-1.2 5.2A1.5 1.5 0 0 1 11.85 13H3.15A1.5 1.5 0 0 1 1.7 11.7L1.5 4Zm1.08 2.5.85 5h8.14l.85-5H2.58Z"/></svg>`;
    case "symlink":
      return `<svg class="vtree-icon-svg" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M9.5 2.5h4v4h-1.5V5.06L8.28 8.78a.75.75 0 1 1-1.06-1.06L10.94 4H9.5v-1.5ZM3 3.5h4.25v1.5H4.5v7h7V8.75H13V13a1 1 0 0 1-1 1H3.5A1 1 0 0 1 2.5 13V4.5a1 1 0 0 1 1-1H3Z"/></svg>`;
    default:
      return `<svg class="vtree-icon-svg" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M4 1.5h5.086a1 1 0 0 1 .707.293l2.414 2.414A1 1 0 0 1 12.5 4.914V13.5A1.5 1.5 0 0 1 11 15H4A1.5 1.5 0 0 1 2.5 13.5v-10A1.5 1.5 0 0 1 4 1.5Zm5 1.5v2h2.5L9 3Zm-5.5 1v9.5A.5.5 0 0 0 4 14h7a.5.5 0 0 0 .5-.5V6H8.25A.75.75 0 0 1 7.5 5.25V3H4a.5.5 0 0 0-.5.5Z"/></svg>`;
  }
}
