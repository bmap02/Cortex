"""Fake Ollama + OpenAI-compatible server for end-to-end tests (no real model needed).

Scripted behaviour: if the last message is from the user, ask for a tool call
(calculator for math questions, write_file when asked to write a file); after a
tool result comes back, stream a final answer that quotes the result. A prompt
containing "loop" repeats the same calculator call until Cortex refuses to rerun it.
"write to <path>" asks to write that file; "slow" streams a long answer slowly (for stopping).
"I learned that <fact>[, related to <note>]" proposes a memory with suggest_memory.
Asked to summarize the conversation (a request without tools), it lists the user's messages.
/api/pull streams download progress and adds the model to /api/tags.
For imports: /api/models/<repo>/tree/main lists a Hugging Face repo's files, /files/model.gguf is a
GGUF to download (slowly, so it can be stopped), and /api/blobs + /api/create take an upload.
Port 0 picks a free port and prints "PORT <n>" first, for tests that run several servers at once.
"""
import hashlib, json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

def decide(messages):
    last = messages[-1]
    if last["role"] == "tool":
        asked = next(m["content"].lower() for m in reversed(messages) if m["role"] == "user")
        # "loop": behave like a small model that repeats the exact same call instead of answering
        if "loop" in asked and not last["content"].startswith("Not run again"):
            return ("tool", "calculator", {"expression": "1234 * 5678"})
        return ("text", f"Done. The tool said: {last['content'].strip()}")
    text = last["content"].lower()
    if "i learned that " in text:
        # "I learned that <fact>[, related to <note>]": propose a memory for review
        fact = last["content"][text.index("i learned that ") + len("i learned that "):].strip()
        fact, _, related = fact.partition(", related to ")
        return ("tool", "suggest_memory", {"fact": fact.strip(), "related": [related.strip()] if related else []})
    if "long" in text:
        para = "This is a deliberately long line that wraps across the terminal width so the row counter has to account for soft wrapping correctly. "
        return ("text", para * 2 + "\n\nWide characters: 日本語テキスト and emoji 🚀🔥 count as two columns.\n" + "\n".join(f"- item {i}" for i in range(8)))
    if "huge" in text:
        return ("text", "\n".join(f"line {i}" for i in range(80)))
    if text.startswith("recall "):
        return ("tool", "recall", {"query": last["content"][len("recall "):].strip()})
    if "remember that " in text:
        at = text.index("remember that ") + len("remember that ")  # same offset in the original text
        return ("tool", "remember", {"fact": last["content"][at:].strip()})
    if "slow" in text:
        return ("text", " ".join(f"word{i}" for i in range(200)))
    if "write" in text:
        path = last["content"].split("write to ", 1)[1].strip() if "write to " in text else "out/hello.txt"
        return ("tool", "write_file", {"path": path, "content": "hello from cortex\n"})
    if any(c.isdigit() for c in text):
        return ("tool", "calculator", {"expression": "1234 * 5678"})
    return ("text", "Hi! I'm a mock model running locally.")

def plain(messages):
    """A reply without tools. Asked for a summary, it lists the user's messages (slowly if one says "lengthy")."""
    if not messages[-1]["content"].startswith("Summarize our conversation"):
        return ("text", "Plain chat answer without tools.")
    asked = [m["content"][:60] for m in messages[:-1] if m["role"] == "user"]
    words = " ".join(f"word{i}" for i in range(200)) + " " if any("lengthy" in a for a in asked) else ""
    return ("text", words + "Summary: the user asked " + " | ".join(asked))

LAST_CHAT = {}  # the last chat request, for tests to see exactly what the model was sent
PULLED = []  # models "downloaded" through /api/pull or created through /api/create
BLOBS = set()  # sha256 digests uploaded to /api/blobs
GGUF = b"GGUF" + bytes(range(256)) * 4096  # a ~1 MB stand-in for a model file
GGUF_CHUNK = 64 * 1024
# a Hugging Face repo with one file per quantization (sizes like a real 7B model)
HF_TREE = [
    {"type": "file", "path": "README.md", "size": 5000},
    {"type": "file", "path": "Qwen2.5-7B-Instruct-Q4_K_M.gguf", "size": 4_683_074_240},
    {"type": "file", "path": "Qwen2.5-7B-Instruct-Q8_0.gguf", "size": 8_098_525_888},
    {"type": "directory", "path": "Qwen2.5-7B-Instruct-f16"},
    {"type": "file", "path": "Qwen2.5-7B-Instruct-f16/Qwen2.5-7B-Instruct-f16-00001-of-00002.gguf", "size": 8_000_000_000},
    {"type": "file", "path": "Qwen2.5-7B-Instruct-f16/Qwen2.5-7B-Instruct-f16-00002-of-00002.gguf", "size": 7_200_000_000},
]

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def _json(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)

    def do_GET(self):
        if self.path == "/api/tags":
            self._json({"models": [{"name": "mock:7b", "size": 4.1e9, "details": {"parameter_size": "7B", "quantization_level": "Q4_K_M"}}]
                       + [{"name": n, "size": 1.9e9, "details": {}} for n in PULLED]})
        elif self.path == "/debug/last-request":
            self._json(LAST_CHAT)
        elif self.path == "/v1/models":
            self._json({"data": [{"id": "mock-7b"}]})
        elif self.path.startswith("/api/models/") and "/tree/main" in self.path:
            self._json(HF_TREE if "Qwen2.5-7B-Instruct-GGUF" in self.path else {"error": "Repository not found"}, 200 if "Qwen2.5-7B-Instruct-GGUF" in self.path else 404)
        elif self.path == "/files/model.gguf":
            self.send_response(200); self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(GGUF))); self.end_headers()
            try:
                for i in range(0, len(GGUF), GGUF_CHUNK):
                    self.wfile.write(GGUF[i:i + GGUF_CHUNK]); self.wfile.flush(); time.sleep(0.02)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pass  # the client stopped the download
        else:
            self._json({"error": "not found"}, 404)

    def do_HEAD(self):
        if self.path == "/files/model.gguf":
            self.send_response(200); self.send_header("Content-Length", str(len(GGUF))); self.end_headers()
        elif self.path.startswith("/api/blobs/"):
            self.send_response(200 if self.path.split(":")[-1] in BLOBS else 404); self.end_headers()
        else:
            self.send_response(404); self.end_headers()

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        if self.path.startswith("/api/blobs/sha256:"):
            # like Ollama: the upload must match the digest it's stored under
            digest = self.path.split(":")[-1]
            if hashlib.sha256(raw).hexdigest() != digest:
                return self._json({"error": "digest mismatch"}, 400)
            BLOBS.add(digest)
            self.send_response(201); self.send_header("Content-Length", "0"); self.end_headers()
            return
        body = json.loads(raw)
        if self.path in ("/api/chat", "/v1/chat/completions"):
            LAST_CHAT.clear(); LAST_CHAT.update(body)
        if self.path == "/api/create":
            missing = [d for d in body.get("files", {}).values() if d.split(":")[-1] not in BLOBS]
            if missing or not body.get("files"):
                return self._json({"error": f"blob not found: {missing}"}, 400)
            self.send_response(200); self.send_header("Content-Type", "application/x-ndjson"); self.end_headers()
            for st in ["parsing GGUF", "using existing layer " + next(iter(body["files"].values())), "writing manifest", "success"]:
                self.wfile.write((json.dumps({"status": st}) + "\n").encode()); self.wfile.flush()
            if body["model"] not in PULLED: PULLED.append(body["model"])
            return
        if self.path == "/api/pull":
            self.send_response(200); self.send_header("Content-Type", "application/x-ndjson"); self.end_headers()
            def send(o): self.wfile.write((json.dumps(o) + "\n").encode()); self.wfile.flush()
            send({"status": "pulling manifest"})
            for done in range(0, 1_000_001, 250_000):
                send({"status": "pulling 6a0746a1ec1a", "digest": "sha256:6a0746a1ec1a", "total": 1_000_000, "completed": done})
                time.sleep(0.05)
            for s in ["verifying sha256 digest", "writing manifest", "success"]: send({"status": s})
            if body["model"] not in PULLED: PULLED.append(body["model"])
            return
        if self.path == "/api/show":
            return self._json({"model_info": {"general.architecture": "qwen2", "qwen2.context_length": 32768}})
        if body.get("model") == "notools" and body.get("tools"):
            return self._json({"error": "registry.ollama.ai/library/notools does not support tools"}, 400)
        d = decide(body["messages"]) if body.get("tools") else plain(body["messages"])
        if self.path == "/api/chat":
            self.send_response(200); self.send_header("Content-Type", "application/x-ndjson"); self.end_headers()
            if d[0] == "tool":
                chunk = {"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": d[1], "arguments": d[2]}}]}, "done": False}
                self.wfile.write((json.dumps(chunk) + "\n").encode())
            else:
                for w in d[1].split(" "):
                    if w == "": continue
                    self.wfile.write((json.dumps({"message": {"role": "assistant", "content": w + " "}, "done": False}) + "\n").encode())
                    self.wfile.flush(); time.sleep(0.05 if w.startswith("word") else 0.01)
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
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 11999), H)
    print(f"PORT {server.server_address[1]}", flush=True)
    server.serve_forever()
