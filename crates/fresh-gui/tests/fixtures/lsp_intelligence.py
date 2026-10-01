#!/usr/bin/env python3
"""Small stdio LSP used by the ADE request bridge integration test."""
import json
from pathlib import Path
import sys
import time

name = sys.argv[1]
log_path = sys.argv[2]
delay = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0
target_uri = Path(sys.argv[4]).as_uri() if len(sys.argv) > 4 else "file:///tmp/target.py"
paged_uri = Path(sys.argv[5]).as_uri() if len(sys.argv) > 5 else target_uri


def write_log(value):
    with open(log_path, "a", encoding="utf-8") as stream:
        stream.write(json.dumps(value, ensure_ascii=False) + "\n")


def send(value):
    body = json.dumps(value).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()


def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line == b"\r\n":
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers["content-length"])))


while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    write_log({"method": method, "params": message.get("params")})
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": {
                "textDocumentSync": 2,
                "completionProvider": {"triggerCharacters": [".", ":"]},
                "hoverProvider": True,
                "signatureHelpProvider": {"triggerCharacters": ["(", ","]},
                "definitionProvider": True,
                "implementationProvider": True,
                "referencesProvider": True,
                "documentSymbolProvider": True,
                "workspaceSymbolProvider": True,
            }
        }})
    elif method == "initialized" or method and method.startswith("textDocument/did"):
        pass
    elif method in ("textDocument/completion", "textDocument/hover", "textDocument/signatureHelp",
                    "textDocument/definition", "textDocument/declaration", "textDocument/typeDefinition",
                    "textDocument/implementation", "textDocument/references", "textDocument/documentSymbol",
                    "workspace/symbol"):
        if delay:
            time.sleep(delay)
        if method == "textDocument/completion":
            result = {"isIncomplete": False, "items": [{
                "label": name + "Completion",
                "insertText": "call($1)\n$0",
                "insertTextFormat": 2,
                "textEdit": {"range": {
                    "start": message["params"]["position"],
                    "end": message["params"]["position"],
                }, "newText": "call($1)\n$0"},
                "additionalTextEdits": [{"range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 0},
                }, "newText": "import package_name\n"}],
                "data": {"completionToken": name},
            }]}
        elif method == "textDocument/hover":
            result = {"contents": {"kind": "markdown", "value": "hover from " + name}}
        elif method == "textDocument/signatureHelp":
            result = {"signatures": [{"label": name + "(value)"}], "activeSignature": 0}
        elif method == "textDocument/definition":
            result = [{"uri": target_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}}]
        elif method == "textDocument/declaration":
            result = [{"targetUri": target_uri, "targetRange": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "targetSelectionRange": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}}]
        elif method == "textDocument/typeDefinition":
            result = {"uri": target_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}}
        elif method == "textDocument/implementation":
            result = [{"uri": target_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}}]
        elif method == "textDocument/references":
            result = [
                {"uri": target_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}},
                {"uri": paged_uri, "range": {"start": {"line": 350000, "character": 3}, "end": {"line": 350000, "character": 4}}},
            ]
        elif method == "textDocument/documentSymbol":
            result = [{"name": "symbol-from-" + name, "kind": 12, "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 5}}, "selectionRange": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 5}}}]
        else:
            result = [{"name": "workspace-" + name, "kind": 12, "location": {"uri": target_uri, "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}}}]
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif "id" in message:
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
