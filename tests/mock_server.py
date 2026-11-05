"""Fake Ollama + OpenAI-compatible server for end-to-end tests (no real model needed).

Scripted behaviour: if the last message is from the user, ask for a tool call
(calculator for math questions, write_file when asked to write a file); after a
tool result comes back, stream a final answer that quotes the result.
"""
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

def decide(messages):
    last = messages[-1]
    if last["role"] == "tool":
        return ("text", f"Done. The tool said: {last['content'].strip()}")
    text = last["content"].lower()
    if "long" in text:
        para = "This is a deliberately long line that wraps across the terminal width so the row counter has to account for soft wrapping correctly. "
        return ("text", para * 2 + "\n\nWide characters: 日本語テキスト and emoji 🚀🔥 count as two columns.\n" + "\n".join(f"- item {i}" for i in range(8)))
    if "huge" in text:
        return ("text", "\n".join(f"line {i}" for i in range(80)))
    if "write" in text:
        return ("tool", "write_file", {"path": "out/hello.txt", "content": "hello from cortex\n"})
    if any(c.isdigit() for c in text):
        return ("tool", "calculator", {"expression": "1234 * 5678"})
    return ("text", "Hi! I'm a mock model running locally.")

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def _json(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)

    def do_GET(self):
        if self.path == "/api/tags":
            self._json({"models": [{"name": "mock:7b", "size": 4.1e9, "details": {"parameter_size": "7B", "quantization_level": "Q4_K_M"}}]})
        elif self.path == "/v1/models":
            self._json({"data": [{"id": "mock-7b"}]})
        else:
            self._json({"error": "not found"}, 404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/api/show":
            return self._json({"model_info": {"general.architecture": "qwen2", "qwen2.context_length": 32768}})
        if body.get("model") == "notools" and body.get("tools"):
            return self._json({"error": "registry.ollama.ai/library/notools does not support tools"}, 400)
        d = decide(body["messages"]) if body.get("tools") else ("text", "Plain chat answer without tools.")
        if self.path == "/api/chat":
            self.send_response(200); self.send_header("Content-Type", "application/x-ndjson"); self.end_headers()
            if d[0] == "tool":
                chunk = {"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": d[1], "arguments": d[2]}}]}, "done": False}
                self.wfile.write((json.dumps(chunk) + "\n").encode())
            else:
                for w in d[1].split(" "):
                    if w == "": continue
                    self.wfile.write((json.dumps({"message": {"role": "assistant", "content": w + " "}, "done": False}) + "\n").encode())
                    self.wfile.flush(); time.sleep(0.01)
            self.wfile.write((json.dumps({"message": {"role": "assistant", "content": ""}, "done": True,
                                          "prompt_eval_count": 321, "eval_count": 40, "eval_duration": 1_000_000_000}) + "\n").encode())
        elif self.path == "/v1/chat/completions":
            self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.end_headers()
            def send(o): self.wfile.write(f"data: {json.dumps(o)}\n\n".encode()); self.wfile.flush()
            if d[0] == "tool":
                args = json.dumps(d[2])
                send({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_abc", "type": "function", "function": {"name": d[1], "arguments": ""}}]}}]})
                for i in range(0, len(args), 7):  # arguments arrive in fragments
                    send({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": args[i:i+7]}}]}}]})
            else:
                for w in d[1].split(" "):
                    send({"choices": [{"delta": {"content": w + " "}}]}); time.sleep(0.01)
            send({"choices": [], "usage": {"prompt_tokens": 300, "completion_tokens": 30}})
            self.wfile.write(b"data: [DONE]\n\n")
        else:
            self._json({"error": "not found"}, 404)

if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 11999), H).serve_forever()
