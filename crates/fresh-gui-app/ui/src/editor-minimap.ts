/**
 * Lazy CodeMirror minimap (document map).
 *
 * Kept in a separate module so Vite can code-split it. Callers must dynamic-import
 * this file only when `ui.editorMinimap` is enabled — when off, this chunk is never
 * fetched and the extension is never mounted.
 */

import type { Extension } from "@codemirror/state";
import type { EditorView } from "@codemirror/view";
import { showMinimap } from "@replit/codemirror-minimap";

/** Build the minimap facet extension (DOM is created only while this is active). */
export function createMinimapExtension(): Extension {
  return showMinimap.of({
    create: (_view: EditorView) => {
      const dom = document.createElement("div");
      dom.className = "cm-minimap-host";
      return { dom };
    },
    displayText: "blocks",
    showOverlay: "always",
  });
}
