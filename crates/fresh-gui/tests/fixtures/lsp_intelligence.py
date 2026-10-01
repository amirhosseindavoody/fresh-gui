#!/usr/bin/env python3
"""Small stdio LSP used by the ADE request bridge integration test."""
import json
import sys
import time

name = sys.argv[1]
log_path = sys.argv[2]
delay = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0


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
            }
        }})
    elif method == "initialized" or method and method.startswith("textDocument/did"):
        pass
    elif method in ("textDocument/completion", "textDocument/hover", "textDocument/signatureHelp"):
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
                "additionalTextEdits": [],
            }]}
        elif method == "textDocument/hover":
            result = {"contents": {"kind": "markdown", "value": "hover from " + name}}
        else:
            result = {"signatures": [{"label": name + "(value)"}], "activeSignature": 0}
        send({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif "id" in message:
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
