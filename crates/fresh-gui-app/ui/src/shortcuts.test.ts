import { describe, expect, test } from "bun:test";
import {
  activeBindingsFromShortkeys,
  matchShortcut,
  parseShortkeyChord,
  setActiveShortkeys,
} from "./shortcuts";
import { shortkeysFromConfigText, stripJsonc } from "./settings";

function keyEvent(
  key: string,
  mods: { ctrl?: boolean; meta?: boolean; shift?: boolean; alt?: boolean } = {},
): KeyboardEvent {
  // bun:test has no DOM KeyboardEvent — minimal stub for matchBinding.
  return {
    key,
    ctrlKey: !!mods.ctrl,
    metaKey: !!mods.meta,
    shiftKey: !!mods.shift,
    altKey: !!mods.alt,
  } as KeyboardEvent;
}

describe("parseShortkeyChord", () => {
  test("parses Mod and Shift", () => {
    const b = parseShortkeyChord("Mod+Shift+P");
    expect(b).not.toBeNull();
    expect(b!.key).toBe("p");
    expect(!!b!.shift).toBe(true);
    expect(!!b!.ctrl || !!b!.meta).toBe(true);
  });

  test("parses Ctrl+Tab", () => {
    const b = parseShortkeyChord("Ctrl+Tab");
    expect(b).toEqual({ key: "Tab", ctrl: true });
  });

  test("parses Alt+Z", () => {
    const b = parseShortkeyChord("Alt+Z");
    expect(b).toEqual({ key: "z", alt: true });
  });
});

describe("activeBindingsFromShortkeys", () => {
  test("falls back to defaults when empty", () => {
    const bindings = activeBindingsFromShortkeys([]);
    expect(bindings.some((b) => b.id === "tab.new")).toBe(true);
  });

  test("loads user shortkeys", () => {
    const bindings = activeBindingsFromShortkeys([
      { action: "tab.new", shortkey: "Mod+N", when: "global" },
    ]);
    expect(bindings).toHaveLength(1);
    expect(bindings[0]!.id).toBe("tab.new");
  });

  test("when: terminal only matches terminal context", () => {
    setActiveShortkeys([
      { action: "tab.close", shortkey: "Mod+W", when: "terminal" },
      { action: "editor.save", shortkey: "Mod+S", when: "editor" },
    ]);
    const modIsMeta = parseShortkeyChord("Mod+W")!.meta === true;
    const closeEv = keyEvent("w", { meta: modIsMeta, ctrl: !modIsMeta });
    const saveEv = keyEvent("s", { meta: modIsMeta, ctrl: !modIsMeta });
    expect(matchShortcut(closeEv, { surface: "terminal" })).toBe("tab.close");
    expect(matchShortcut(closeEv, { surface: "editor" })).toBeNull();
    expect(matchShortcut(saveEv, { surface: "editor" })).toBe("editor.save");
    expect(matchShortcut(saveEv, { surface: "terminal" })).toBeNull();
  });
});

describe("shortkeysFromConfigText", () => {
  test("parses JSONC shortkeys", () => {
    const text = `{
      // comment
      "shortkeys": [
        { "action": "settings.open", "shortkey": "Mod+,", "when": "global" }
      ]
    }`;
    expect(stripJsonc(text)).not.toContain("//");
    const keys = shortkeysFromConfigText(text);
    expect(keys).toEqual([
      { action: "settings.open", shortkey: "Mod+,", when: "global" },
    ]);
  });
});
