#!/usr/bin/env python3
"""Sleep far longer than the CGI deadline, to show the timeout works.

While this runs the server must keep answering every other client: that is the
whole point of putting the pipes in the same epoll instead of blocking on the
child.
"""
import time
print("Content-Type: text/plain")
print()
print("this will never be delivered")
time.sleep(120)
