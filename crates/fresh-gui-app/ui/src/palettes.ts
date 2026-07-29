/**
 * Host color palettes.
 *
 * Named packs (nord, dracula, …) reuse colors from Fresh editor themes under
 * `vendor/fresh/crates/fresh-editor/themes/*.json` — mapped onto fresh-gui CSS
 * tokens (chrome, xterm, CodeMirror). `primer` keeps the built-in Primer
 * light/dark tokens in `tokens.css` and respects `ui.theme`.
 */

export const PALETTE_IDS = [
  "primer",
  "nord",
  "dracula",
  "solarized-dark",
  "high-contrast",
  "nostalgia",
  "dark",
  "light",
] as const;

export type PaletteId = (typeof PALETTE_IDS)[number];

export type ResolvedAppearance = "dark" | "light";

/** CSS custom properties we may override when applying a non-primer palette. */
export const PALETTE_CSS_VARS = [
  "--bg",
  "--bg-elevated",
  "--panel",
  "--panel-2",
  "--border",
  "--border-strong",
  "--text",
  "--muted",
  "--accent",
  "--accent-dim",
  "--danger",
  "--warning",
  "--success",
  "--focus-ring",
  "--term-bg",
  "--term-fg",
  "--term-cursor",
  "--term-selection",
  "--term-selection-fg",
  "--editor-bg",
  "--editor-fg",
  "--editor-gutter-bg",
  "--editor-gutter-fg",
  "--editor-active-line",
  "--editor-selection",
  "--editor-cursor",
  "--editor-matching-bracket",
  "--syn-keyword",
  "--syn-control",
  "--syn-string",
  "--syn-comment",
  "--syn-number",
  "--syn-bool",
  "--syn-function",
  "--syn-type",
  "--syn-variable",
  "--syn-property",
  "--syn-operator",
  "--syn-punctuation",
  "--syn-tag",
  "--syn-attribute",
  "--syn-heading",
  "--syn-link",
  "--syn-meta",
  "--syn-invalid",
  "--list-hover",
  "--list-active",
  "--background",
  "--foreground",
  "--card",
  "--card-foreground",
  "--popover",
  "--popover-foreground",
  "--primary",
  "--primary-foreground",
  "--secondary",
  "--secondary-foreground",
  "--muted-foreground",
  "--accent-foreground",
  "--destructive",
  "--input",
  "--ring",
] as const;

type Rgb = readonly [number, number, number];

type TokenMap = Record<(typeof PALETTE_CSS_VARS)[number], string>;

type PaletteDef = {
  /** Human label for docs / status. */
  label: string;
  /** Forced appearance for non-primer packs (Fresh themes are not OS-adaptive). */
  appearance: ResolvedAppearance;
  tokens: TokenMap;
};

function hex([r, g, b]: Rgb, alpha?: number): string {
  if (alpha == null || alpha >= 1) {
    return `#${[r, g, b].map((n) => n.toString(16).padStart(2, "0")).join("")}`;
  }
  const a = Math.round(Math.min(1, Math.max(0, alpha)) * 255)
    .toString(16)
    .padStart(2, "0");
  return `#${[r, g, b].map((n) => n.toString(16).padStart(2, "0")).join("")}${a}`;
}

function mix(base: Rgb, amount: number, toward: Rgb = [0, 0, 0]): Rgb {
  const t = Math.min(1, Math.max(0, amount));
  return [
    Math.round(base[0] + (toward[0] - base[0]) * t),
    Math.round(base[1] + (toward[1] - base[1]) * t),
    Math.round(base[2] + (toward[2] - base[2]) * t),
  ] as const;
}

/** Build a full token map from Fresh-style editor + syntax + accent RGB. */
function fromFresh(opts: {
  bg: Rgb;
  fg: Rgb;
  cursor: Rgb;
  selection: Rgb;
  line: Rgb;
  gutterFg: Rgb;
  gutterBg: Rgb;
  bracket: Rgb;
  accent: Rgb;
  danger?: Rgb;
  warning?: Rgb;
  success?: Rgb;
  keyword: Rgb;
  string: Rgb;
  comment: Rgb;
  function: Rgb;
  type: Rgb;
  variable: Rgb;
  constant: Rgb;
  operator: Rgb;
  punct?: Rgb;
  appearance: ResolvedAppearance;
}): TokenMap {
  const {
    bg,
    fg,
    cursor,
    selection,
    line,
    gutterFg,
    gutterBg,
    bracket,
    accent,
    danger = [248, 81, 73] as Rgb,
    warning = [210, 153, 34] as Rgb,
    success = [63, 185, 80] as Rgb,
    keyword,
    string,
    comment,
    function: fn,
    type,
    variable,
    constant,
    operator,
    punct = fg,
    appearance,
  } = opts;

  const elevated = mix(bg, appearance === "dark" ? 0.08 : 0.04, appearance === "dark" ? [255, 255, 255] : [0, 0, 0]);
  const panel = mix(bg, appearance === "dark" ? 0.14 : 0.06, appearance === "dark" ? [255, 255, 255] : [0, 0, 0]);
  const panel2 = mix(bg, appearance === "dark" ? 0.22 : 0.1, appearance === "dark" ? [255, 255, 255] : [0, 0, 0]);
  const border = mix(fg, appearance === "dark" ? 0.72 : 0.78, bg);
  const borderStrong = mix(fg, appearance === "dark" ? 0.55 : 0.6, bg);
  const muted = mix(fg, 0.35, bg);
  const primaryFg: Rgb = appearance === "dark" ? [255, 255, 255] : [255, 255, 255];

  return {
    "--bg": hex(bg),
    "--bg-elevated": hex(elevated),
    "--panel": hex(panel),
    "--panel-2": hex(panel2),
    "--border": hex(border),
    "--border-strong": hex(borderStrong),
    "--text": hex(fg),
    "--muted": hex(muted),
    "--accent": hex(accent),
    "--accent-dim": hex(mix(accent, 0.78, bg)),
    "--danger": hex(danger),
    "--warning": hex(warning),
    "--success": hex(success),
    "--focus-ring": hex(accent, 0.55),
    "--term-bg": hex(bg),
    "--term-fg": hex(fg),
    "--term-cursor": hex(cursor),
    "--term-selection": hex(selection, 0.7),
    "--term-selection-fg": hex(fg),
    "--editor-bg": hex(bg),
    "--editor-fg": hex(fg),
    "--editor-gutter-bg": hex(gutterBg),
    "--editor-gutter-fg": hex(gutterFg),
    "--editor-active-line": hex(line, 0.85),
    "--editor-selection": hex(selection, 0.55),
    "--editor-cursor": hex(cursor),
    "--editor-matching-bracket": hex(bracket, 0.4),
    "--syn-keyword": hex(keyword),
    "--syn-control": hex(keyword),
    "--syn-string": hex(string),
    "--syn-comment": hex(comment),
    "--syn-number": hex(constant),
    "--syn-bool": hex(constant),
    "--syn-function": hex(fn),
    "--syn-type": hex(type),
    "--syn-variable": hex(variable),
    "--syn-property": hex(constant),
    "--syn-operator": hex(operator),
    "--syn-punctuation": hex(punct),
    "--syn-tag": hex(fn),
    "--syn-attribute": hex(constant),
    "--syn-heading": hex(accent),
    "--syn-link": hex(constant),
    "--syn-meta": hex(comment),
    "--syn-invalid": hex(danger),
    "--list-hover": hex(mix(fg, 0.92, bg)),
    "--list-active": hex(mix(accent, 0.78, bg)),
    "--background": hex(bg),
    "--foreground": hex(fg),
    "--card": hex(panel),
    "--card-foreground": hex(fg),
    "--popover": hex(elevated),
    "--popover-foreground": hex(fg),
    "--primary": hex(accent),
    "--primary-foreground": hex(primaryFg),
    "--secondary": hex(panel2),
    "--secondary-foreground": hex(fg),
    "--muted-foreground": hex(muted),
    "--accent-foreground": hex(fg),
    "--destructive": hex(danger),
    "--input": hex(border),
    "--ring": hex(accent),
  };
}

/**
 * Fresh theme colors → host tokens.
 * Source: `vendor/fresh/crates/fresh-editor/themes/<id>.json`
 */
const FRESH_PALETTES: Record<Exclude<PaletteId, "primer">, PaletteDef> = {
  nord: {
    label: "Nord",
    appearance: "dark",
    tokens: fromFresh({
      appearance: "dark",
      bg: [46, 52, 64],
      fg: [216, 222, 233],
      cursor: [136, 192, 208],
      selection: [80, 92, 116],
      line: [59, 66, 82],
      gutterFg: [107, 118, 140],
      gutterBg: [46, 52, 64],
      bracket: [129, 161, 193],
      accent: [136, 192, 208],
      danger: [191, 97, 106],
      warning: [235, 203, 139],
      success: [163, 190, 140],
      keyword: [129, 161, 193],
      string: [163, 190, 140],
      comment: [107, 118, 140],
      function: [136, 192, 208],
      type: [143, 188, 187],
      variable: [216, 222, 233],
      constant: [180, 142, 173],
      operator: [129, 161, 193],
      punct: [180, 188, 200],
    }),
  },
  dracula: {
    label: "Dracula",
    appearance: "dark",
    tokens: fromFresh({
      appearance: "dark",
      bg: [40, 42, 54],
      fg: [248, 248, 242],
      cursor: [255, 121, 198],
      selection: [68, 71, 90],
      line: [50, 52, 66],
      gutterFg: [98, 114, 164],
      gutterBg: [40, 42, 54],
      bracket: [255, 121, 198],
      accent: [189, 147, 249],
      danger: [255, 85, 85],
      warning: [241, 250, 140],
      success: [80, 250, 123],
      keyword: [255, 121, 198],
      string: [241, 250, 140],
      comment: [98, 114, 164],
      function: [80, 250, 123],
      type: [139, 233, 253],
      variable: [248, 248, 242],
      constant: [189, 147, 249],
      operator: [255, 121, 198],
    }),
  },
  "solarized-dark": {
    label: "Solarized Dark",
    appearance: "dark",
    tokens: fromFresh({
      appearance: "dark",
      bg: [0, 43, 54],
      fg: [131, 148, 150],
      cursor: [38, 139, 210],
      selection: [22, 72, 86],
      line: [7, 54, 66],
      gutterFg: [101, 123, 131],
      gutterBg: [0, 43, 54],
      bracket: [131, 148, 150],
      accent: [38, 139, 210],
      danger: [220, 50, 47],
      warning: [181, 137, 0],
      success: [133, 153, 0],
      keyword: [133, 153, 0],
      string: [42, 161, 152],
      comment: [101, 123, 131],
      function: [38, 139, 210],
      type: [181, 137, 0],
      variable: [131, 148, 150],
      constant: [203, 75, 22],
      operator: [131, 148, 150],
      punct: [147, 161, 161],
    }),
  },
  "high-contrast": {
    label: "High Contrast",
    appearance: "dark",
    tokens: fromFresh({
      appearance: "dark",
      bg: [0, 0, 0],
      fg: [255, 255, 255],
      cursor: [255, 255, 255],
      selection: [50, 60, 90],
      line: [20, 20, 20],
      gutterFg: [140, 140, 140],
      gutterBg: [0, 0, 0],
      bracket: [255, 255, 255],
      accent: [255, 255, 0],
      danger: [255, 85, 85],
      warning: [255, 255, 0],
      success: [0, 205, 0],
      keyword: [0, 255, 255],
      string: [0, 205, 0],
      comment: [229, 229, 229],
      function: [255, 255, 0],
      type: [255, 85, 255],
      variable: [255, 255, 255],
      constant: [120, 130, 255],
      operator: [255, 255, 255],
    }),
  },
  nostalgia: {
    label: "Nostalgia",
    appearance: "dark",
    tokens: fromFresh({
      appearance: "dark",
      bg: [0, 0, 170],
      fg: [255, 255, 85],
      cursor: [255, 255, 255],
      selection: [30, 30, 200],
      line: [0, 0, 128],
      gutterFg: [85, 255, 255],
      gutterBg: [0, 0, 170],
      bracket: [170, 170, 170],
      accent: [0, 170, 170],
      danger: [255, 85, 85],
      warning: [255, 255, 85],
      success: [0, 255, 0],
      keyword: [255, 255, 255],
      string: [0, 255, 255],
      comment: [128, 128, 128],
      function: [255, 255, 0],
      type: [0, 255, 0],
      variable: [255, 255, 85],
      constant: [255, 0, 255],
      operator: [170, 170, 170],
    }),
  },
  dark: {
    label: "Fresh Dark",
    appearance: "dark",
    tokens: fromFresh({
      appearance: "dark",
      bg: [30, 30, 30],
      fg: [212, 212, 212],
      cursor: [255, 255, 255],
      selection: [50, 50, 60],
      line: [40, 40, 40],
      gutterFg: [100, 100, 100],
      gutterBg: [30, 30, 30],
      bracket: [212, 212, 212],
      accent: [86, 156, 214],
      keyword: [86, 156, 214],
      string: [206, 145, 120],
      comment: [106, 153, 85],
      function: [220, 220, 170],
      type: [78, 201, 176],
      variable: [156, 220, 254],
      constant: [79, 193, 255],
      operator: [212, 212, 212],
    }),
  },
  light: {
    label: "Fresh Light",
    appearance: "light",
    tokens: fromFresh({
      appearance: "light",
      bg: [255, 255, 255],
      fg: [0, 0, 0],
      cursor: [0, 0, 0],
      selection: [225, 232, 242],
      line: [245, 245, 245],
      gutterFg: [115, 115, 115],
      gutterBg: [255, 255, 255],
      bracket: [0, 0, 0],
      accent: [0, 112, 193],
      danger: [207, 34, 46],
      warning: [154, 103, 0],
      success: [26, 127, 55],
      keyword: [175, 0, 219],
      string: [163, 21, 21],
      comment: [0, 128, 0],
      function: [121, 94, 38],
      type: [0, 128, 128],
      variable: [0, 90, 140],
      constant: [0, 112, 193],
      operator: [0, 0, 0],
      punct: [50, 50, 50],
    }),
  },
};

export function parsePaletteId(value: unknown): PaletteId {
  if (typeof value === "string") {
    const id = value.trim().toLowerCase();
    if ((PALETTE_IDS as readonly string[]).includes(id)) return id as PaletteId;
  }
  return "primer";
}

export function paletteLabel(id: PaletteId): string {
  if (id === "primer") return "Primer";
  return FRESH_PALETTES[id].label;
}

export function listPalettes(): Array<{ id: PaletteId; label: string }> {
  return PALETTE_IDS.map((id) => ({ id, label: paletteLabel(id) }));
}

function clearPaletteOverrides(root: HTMLElement = document.documentElement): void {
  for (const name of PALETTE_CSS_VARS) {
    root.style.removeProperty(name);
  }
}

/**
 * Apply a palette onto `:root`.
 * - `primer`: clear overrides; caller keeps `data-theme` from `ui.theme`.
 * - Fresh packs: write CSS vars from Fresh theme JSON colors; set `data-theme`
 *   to the pack appearance so residual chrome matches.
 */
export function applyPalette(
  id: PaletteId,
  root: HTMLElement = document.documentElement,
): ResolvedAppearance | null {
  root.dataset.palette = id;
  if (id === "primer") {
    clearPaletteOverrides(root);
    return null;
  }
  const pack = FRESH_PALETTES[id];
  for (const [name, value] of Object.entries(pack.tokens)) {
    root.style.setProperty(name, value);
  }
  root.dataset.theme = pack.appearance;
  return pack.appearance;
}
