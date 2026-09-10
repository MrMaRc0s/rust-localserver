#!/usr/bin/env python3
"""Echo the CGI environment and the request body.

Proves the plumbing end to end: the variables the server set, the body it
piped to stdin, and the working directory it chose (which must be this
script's own directory, so `open("data.txt")` below resolves next to the
script rather than next to wherever the server was started).
"""
import os
import sys
import html

body = sys.stdin.read()

# Relative path, on purpose: this only works if the server set current_dir to
# the script's directory.
try:
    with open("data.txt") as handle:
        relative_read = handle.read().strip()
except OSError as error:
    relative_read = f"could not read data.txt: {error}"

interesting = [
    "REQUEST_METHOD", "SCRIPT_NAME", "SCRIPT_FILENAME", "PATH_INFO",
    "PATH_TRANSLATED", "QUERY_STRING", "CONTENT_TYPE", "CONTENT_LENGTH",
    "SERVER_NAME", "SERVER_PORT", "SERVER_PROTOCOL", "GATEWAY_INTERFACE",
    "REMOTE_ADDR", "DOCUMENT_ROOT",
]

print("Content-Type: text/html; charset=utf-8")
print("X-Cgi-Language: python")
print()
print("<!doctype html><html><head><meta charset='utf-8'>")
print("<title>CGI — python</title>")
print("<style>body{margin:0;padding:2rem 1.5rem;background:#10151c;color:#e6edf3;"
      "font:15px/1.6 system-ui,-apple-system,sans-serif}main{max-width:46rem;margin:0 auto}"
      "h1{font-size:1.3rem}table{width:100%;border-collapse:collapse;margin:1rem 0}"
      "th,td{text-align:left;padding:.35rem .6rem;border-bottom:1px solid #2a3441}"
      "th{color:#8b98a5;width:12rem}code{font-family:ui-monospace,Menlo,monospace;font-size:.88em}"
      "a{color:#58a6ff}</style></head><body><main>")
print("<h1>CGI is running (python)</h1>")
print(f"<p>Working directory: <code>{html.escape(os.getcwd())}</code></p>")
print(f"<p>Relative <code>open(\"data.txt\")</code> read: "
      f"<code>{html.escape(relative_read)}</code></p>")
print("<table>")
for name in interesting:
    value = os.environ.get(name, "")
    print(f"<tr><th>{name}</th><td><code>{html.escape(value)}</code></td></tr>")
print("</table>")
print(f"<h2>Body ({len(body)} bytes)</h2>")
print(f"<pre><code>{html.escape(body) if body else '(empty)'}</code></pre>")
http_vars = sorted(k for k in os.environ if k.startswith("HTTP_"))
print("<h2>Request headers as HTTP_*</h2><table>")
for name in http_vars:
    print(f"<tr><th>{name}</th><td><code>{html.escape(os.environ[name])}</code></td></tr>")
print("</table>")
print("<p><a href='/'>Back to the index</a></p>")
print("</main></body></html>")
