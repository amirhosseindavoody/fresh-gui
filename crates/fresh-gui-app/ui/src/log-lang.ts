import { StreamLanguage } from "@codemirror/language";
import { tags as t } from "@lezer/highlight";

/** Lightweight highlighter for application / build log files. */
export const logLanguage = StreamLanguage.define({
  name: "log",
  tokenTable: {
    logError: t.invalid,
    logWarn: t.number,
    logInfo: t.keyword,
    logDebug: t.comment,
    logTime: t.meta,
    logPath: t.string,
    logUrl: t.link,
  },
  token(stream) {
    if (stream.eatSpace()) return null;

    if (
      stream.match(/^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?/) ||
      stream.match(/^\[\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}[^\]]*\]/) ||
      stream.match(/^\d{2}:\d{2}:\d{2}(?:[.,]\d+)?/)
    ) {
      return "logTime";
    }

    const level = stream.match(
      /^\[?(ERROR|FATAL|CRITICAL|WARN(?:ING)?|INFO|DEBUG|TRACE|NOTICE)\]?\b/i,
    );
    if (level) {
      const name = (Array.isArray(level) ? String(level[1] ?? "") : "").toUpperCase();
      if (name === "ERROR" || name === "FATAL" || name === "CRITICAL") return "logError";
      if (name === "WARN" || name === "WARNING") return "logWarn";
      if (name === "DEBUG" || name === "TRACE") return "logDebug";
      return "logInfo";
    }

    if (stream.match(/^https?:\/\/\S+/)) return "logUrl";
    if (stream.match(/^"(?:[^"\\]|\\.)*"/) || stream.match(/^'(?:[^'\\]|\\.)*'/)) return "string";
    if (stream.match(/^0x[0-9a-fA-F]+\b/) || stream.match(/^\d+(?:\.\d+)?\b/)) return "number";
    if (
      stream.match(/^(?:\/|~\/|\.\.?\/)[\w./@+_-]+/) ||
      stream.match(/^[A-Za-z]:\\[\w.\\_-]+/)
    ) {
      return "logPath";
    }

    if (stream.match(/^[A-Za-z_][\w.-]*/)) return null;
    stream.next();
    return null;
  },
});
