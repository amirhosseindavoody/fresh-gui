import { useEffect } from "react";
import {
  ArrowUp,
  Files,
  Menu,
  Palette,
  PanelLeftClose,
  Plus,
  Settings,
} from "lucide-react";
import { bootstrapAde } from "@/ade/bootstrap";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { cn } from "@/lib/utils";

/**
 * Host ADE chrome. IDs must stay stable for the imperative controller in
 * `bootstrapAde` (xterm / CodeMirror / VirtualTree attach by id).
 *
 * Keep this tree static after mount: ADE mutates disabled/hidden/classList on
 * these nodes. Avoid wrapping them in Radix state (tooltips/menus) that would
 * re-render and clobber those attributes.
 */
export function App() {
  useEffect(() => {
    bootstrapAde();
  }, []);

  return (
    <>
      <div id="workspace" className="workspace">
        <nav id="activity-bar" className="activity-bar" aria-label="Activity">
          <Button
            id="activity-explorer"
            className={cn("activity-btn", "active")}
            type="button"
            variant="ghost"
            size="icon"
            title="Explorer (Mod+B)"
            aria-pressed={true}
          >
            <Files className="activity-glyph size-4" aria-hidden />
          </Button>
          <Button
            id="activity-palette"
            className="activity-btn"
            type="button"
            variant="ghost"
            size="icon"
            title="Color palette (Mod+Shift+P → Color Palette)"
          >
            <Palette className="activity-glyph size-4" aria-hidden />
          </Button>
          <div className="activity-spacer" />
          <Button
            id="activity-settings"
            className="activity-btn"
            type="button"
            variant="ghost"
            size="icon"
            title="Settings (Mod+,)"
          >
            <Settings className="activity-glyph size-4" aria-hidden />
          </Button>
        </nav>

        <aside id="sidebar" className="sidebar">
          <div className="sidebar-head">
            <h2 id="sidebar-title">Explorer</h2>
            <div className="sidebar-head-actions">
              <Button
                id="sidebar-parent"
                type="button"
                variant="ghost"
                size="icon-xs"
                disabled
                title="Go to parent folder"
                aria-label="Go to parent folder"
              >
                <ArrowUp aria-hidden />
              </Button>
              <Button
                id="sidebar-toggle"
                type="button"
                variant="ghost"
                size="icon-xs"
                title="Toggle sidebar (Mod+B)"
              >
                <PanelLeftClose aria-hidden />
              </Button>
            </div>
          </div>
          <div id="tree" className="tree" />
        </aside>
        <div id="sidebar-resizer" className="sidebar-resizer" title="Drag to resize" />

        <div id="main-col" className="main-col">
          <div id="tabs-bar" className="tabs-bar">
            <div className="tabs-actions tabs-actions-start">
              <Button
                id="tabs-menu"
                type="button"
                variant="ghost"
                size="icon-xs"
                disabled
                title="Tab actions"
                aria-label="Tab actions"
                aria-haspopup="menu"
              >
                <Menu aria-hidden />
              </Button>
            </div>
            <Separator orientation="vertical" className="mx-1 h-5 self-center" />
            <div id="tabs" className="tabs" role="tablist">
              <div id="tab-pill" className="tab-pill" hidden />
            </div>
            <div className="tabs-actions tabs-actions-end">
              <Button
                id="new-tab"
                type="button"
                variant="ghost"
                size="icon-xs"
                disabled
                title="New tab"
                aria-label="New tab"
                aria-haspopup="menu"
              >
                <Plus aria-hidden />
              </Button>
            </div>
          </div>

          <div id="stacks" className="stacks">
            <div id="empty-stack" className="empty-stack">
              Open the printed Local access URL to connect
            </div>
            <div id="terminal-stack" className="stack" hidden>
              <div id="panes" className="pane-tree" />
            </div>
            <div id="editor-stack" className="stack" hidden />
          </div>
        </div>
      </div>

      <footer id="statusbar" className="statusbar">
        <span id="status-left">disconnected</span>
        <span id="status-right" />
      </footer>
    </>
  );
}
