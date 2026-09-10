#!/usr/bin/env python3
"""Read a POSTed form body and echo it back.

Exercises the stdin pipe specifically: the server writes the request body into
this script one chunk per writable event, and the script reads it to EOF.
"""
import os
import sys
import html
import urllib.parse

length = os.environ.get("CONTENT_LENGTH", "")
body = sys.stdin.read()
fields = urllib.parse.parse_qs(body) if body else {}

print("Content-Type: text/html; charset=utf-8")
print()
print("<!doctype html><html><head><meta charset='utf-8'><title>CGI form</title>")
print("<style>body{margin:0;padding:2rem 1.5rem;background:#10151c;color:#e6edf3;"
      "font:15px/1.6 system-ui,sans-serif}main{max-width:40rem;margin:0 auto}"
      "input,button{font:inherit;padding:.4rem .6rem;border-radius:6px;border:1px solid #2a3441;"
      "background:#182029;color:#e6edf3}button{border-color:#58a6ff;color:#58a6ff;cursor:pointer}"
      "code{font-family:ui-monospace,Menlo,monospace}a{color:#58a6ff}</style>")
print("</head><body><main>")
print("<h1>CGI form</h1>")
print(f"<p>Method: <code>{html.escape(os.environ.get('REQUEST_METHOD', '?'))}</code>, "
      f"CONTENT_LENGTH: <code>{html.escape(length) or '(none)'}</code>, "
      f"bytes actually read: <code>{len(body)}</code></p>")
if fields:
    print("<h2>Fields received</h2><ul>")
    for key, values in sorted(fields.items()):
        for value in values:
            print(f"<li><code>{html.escape(key)}</code> = <code>{html.escape(value)}</code></li>")
    print("</ul>")
else:
    print("<p>No fields yet — submit the form.</p>")
print("<form method='POST' action='/cgi-bin/form.py'>")
print("<input name='message' placeholder='Say something'> <button>POST it</button>")
print("</form>")
print("<p><a href='/'>Back to the index</a></p>")
print("</main></body></html>")
