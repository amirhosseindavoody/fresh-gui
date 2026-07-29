/** Lightweight file-type badges for the explorer (no icon pack dependency). */

const EXT_BADGE: Record<string, string> = {
  rs: "rs",
  ts: "ts",
  tsx: "tx",
  js: "js",
  jsx: "jx",
  mjs: "js",
  cjs: "js",
  py: "py",
  md: "md",
  json: "{}",
  toml: "tm",
  yaml: "y",
  yml: "y",
  css: "#",
  scss: "#",
  html: "<>",
  svg: "◇",
  sh: "$",
  bash: "$",
  zsh: "$",
  go: "go",
  c: "c",
  h: "h",
  cpp: "c+",
  hpp: "h+",
  java: "jv",
  kt: "kt",
  rb: "rb",
  php: "ph",
  sql: "sq",
  lock: "lk",
  txt: "·",
  rsx: "rx",
};

export function fileIcon(name: string, kind: string): string {
  if (kind === "dir") return "";
  if (kind === "symlink") return "↗";
  const base = name.includes(".") ? name.split(".").pop()!.toLowerCase() : "";
  if (!base) return "·";
  return EXT_BADGE[base] || "·";
}
