"""A stand-in for the Telegram Bot API: records each sendMessage and editMessageText body as a JSON line."""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        ok = self.path.startswith("/bot") and self.path.endswith(("/sendMessage", "/editMessageText"))
        message_id = 0
        if ok:
            with open(self.server.messages, "a+") as messages:
                messages.write(json.dumps(body) + "\n")
                messages.seek(0)
                message_id = body.get("message_id") or len(messages.readlines())
        reply = json.dumps(
            {"ok": ok, "result": {"message_id": message_id}, "description": "" if ok else "not found"}
        ).encode()
        self.send_response(200 if ok else 404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, format: str, *args) -> None:
        pass


def main() -> None:
    port, messages = int(sys.argv[1]), sys.argv[2]
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.messages = messages
    server.serve_forever()


if __name__ == "__main__":
    main()
