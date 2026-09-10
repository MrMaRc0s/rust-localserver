#!/usr/bin/env bash
#
# Audit Walkthrough Script: tests all audit questions [A-9] through [A-28]
# against LocalServer.
#

set -u

GREEN='\033[0;32m'
RED='\033[0;31m'
BLUE='\033[0;34m'
RESET='\033[0m'
PASS="${GREEN}[PASS]${RESET}"
FAIL="${RED}[FAIL]${RESET}"

TOTAL=0
PASSED=0
FAILED=0

record_result() {
    local q_id="$1"
    local desc="$2"
    local status="$3"
    TOTAL=$((TOTAL + 1))
    if [ "$status" -eq 0 ]; then
        echo -e "${PASS} [${q_id}] ${desc}"
        PASSED=$((PASSED + 1))
    else
        echo -e "${FAIL} [${q_id}] ${desc}"
        FAILED=$((FAILED + 1))
    fi
}

SERVER_STARTED_BY_SCRIPT=0
cleanup() {
    rm -f /tmp/audit_cookies*.txt /tmp/audit_*.bin /tmp/audit_*.txt
    if [ "$SERVER_STARTED_BY_SCRIPT" -eq 1 ] && [ -n "${SERVER_PID:-}" ]; then
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# Ensure binary is built
if [ ! -f ./target/debug/localserver ] && [ ! -f ./target/release/localserver ]; then
    cargo build --quiet
fi
BINARY="./target/debug/localserver"
[ -f ./target/release/localserver ] && BINARY="./target/release/localserver"

# Check if ports 8080, 8081, 8082 are open. If not, start server in background.
if ! nc -z 127.0.0.1 8080 2>/dev/null; then
    echo -e "${BLUE}Starting localserver in background...${RESET}"
    $BINARY config.yaml >/dev/null 2>&1 &
    SERVER_PID=$!
    SERVER_STARTED_BY_SCRIPT=1

    # Wait for ports to become ready
    for i in {1..50}; do
        if nc -z 127.0.0.1 8080 2>/dev/null && nc -z 127.0.0.1 8081 2>/dev/null && nc -z 127.0.0.1 8082 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
fi

echo "============================================================"
echo "LocalServer Audit Test Suite [A-9] .. [A-28]"
echo "Target: http://127.0.0.1:8080, 8081, 8082"
echo "============================================================"

# [A-9] Setup a single server with a single port.
code=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8081/)
if [ "$code" = "200" ]; then
    record_result "A-9" "Single server, single port (port 8081 returns 200)" 0
else
    record_result "A-9" "Single server, single port (expected 200, got $code)" 1
fi

# [A-10] Setup multiple servers with different ports.
c8080=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/)
c8081=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8081/)
c8082=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8082/)
if [ "$c8080" = "200" ] && [ "$c8081" = "200" ] && [ "$c8082" = "200" ]; then
    record_result "A-10" "Multiple servers with different ports (8080, 8081, 8082 all respond 200)" 0
else
    record_result "A-10" "Multiple servers with different ports (8080:$c8080, 8081:$c8081, 8082:$c8082)" 1
fi

# [A-11] Setup multiple servers with different hostnames.
body_lh=$(curl -s http://127.0.0.1:8080/)
body_tc=$(curl -s --resolve test.com:8080:127.0.0.1 http://test.com:8080/)
if echo "$body_lh" | grep -q "LocalServer" && echo "$body_tc" | grep -q "test.com — virtual host"; then
    record_result "A-11" "Multiple servers with different hostnames (localhost vs test.com)" 0
else
    record_result "A-11" "Multiple servers with different hostnames failed to distinguish content" 1
fi

# [A-12] Setup custom error pages.
out_404=$(curl -s -i http://127.0.0.1:8080/nonexistent-route-page)
if echo "$out_404" | grep -q "404 Not Found" && echo "$out_404" | grep -q "No route or file matches this URL on this server."; then
    record_result "A-12" "Custom error pages (404 served from error_pages/404.html)" 0
else
    record_result "A-12" "Custom error pages (custom 404 body missing)" 1
fi

# [A-13] Limit the client body (1K limit on port 8082).
data_2k=$(head -c 2048 /dev/zero | tr '\0' 'X')
code_413=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "Content-Type: text/plain" --data "$data_2k" http://127.0.0.1:8082/)
data_256=$(head -c 256 /dev/zero | tr '\0' 'Y')
code_ok=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "Content-Type: text/plain" --data "$data_256" http://127.0.0.1:8082/)
if [ "$code_413" = "413" ] && [ "$code_ok" != "413" ]; then
    record_result "A-13" "Limit client body (>1K returns 413, <=1K accepted on 8082)" 0
else
    record_result "A-13" "Limit client body (expected 413/accepted, got $code_413/$code_ok)" 1
fi

# [A-14] Setup routes and ensure they are taken into account.
r_root=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/)
r_listing=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/listing/)
r_uploads=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/uploads/)
r_readonly=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/readonly/)
r_cgi=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/cgi-bin/hello.py)
r_redirect=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/old-page)
if [ "$r_root" = "200" ] && [ "$r_listing" = "200" ] && [ "$r_uploads" = "200" ] && \
   [ "$r_readonly" = "200" ] && [ "$r_cgi" = "200" ] && [ "$r_redirect" = "301" ]; then
    record_result "A-14" "Configured routes are all resolved properly (/, /listing, /uploads, /readonly, /cgi-bin, /old-page)" 0
else
    record_result "A-14" "Configured routes route resolution failed" 1
fi

# [A-15] Setup a default file in case the path is a directory.
dir_resp=$(curl -s http://127.0.0.1:8080/)
if echo "$dir_resp" | grep -q "<title>LocalServer</title>"; then
    record_result "A-15" "Default file for a directory (serves index.html on /)" 0
else
    record_result "A-15" "Default file for a directory (failed to serve index.html)" 1
fi

# [A-16] Accepted methods per route (DELETE with permission on /uploads, without on /readonly).
echo "test delete" > uploads/audit_test_perm.txt
del_perm=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE http://127.0.0.1:8080/uploads/audit_test_perm.txt)
del_header=$(curl -s -i -X DELETE http://127.0.0.1:8080/readonly/audit_test_perm.txt)
if [ "$del_perm" = "204" ] && echo "$del_header" | grep -q "405 Method Not Allowed" && echo "$del_header" | grep -qi "Allow:.*GET"; then
    record_result "A-16" "Accepted methods per route (204 on /uploads/, 405 + Allow on /readonly/)" 0
else
    record_result "A-16" "Accepted methods per route failed" 1
fi

# [A-17] Are the GET requests working properly?
get_headers=$(curl -s -i http://127.0.0.1:8080/index.html)
if echo "$get_headers" | grep -q "200 OK" && echo "$get_headers" | grep -qi "Content-Type: text/html" && \
   echo "$get_headers" | grep -qi "Content-Length:" && echo "$get_headers" | grep -qi "Date:"; then
    record_result "A-17" "GET requests working properly (200 OK, Content-Type, Content-Length, Date)" 0
else
    record_result "A-17" "GET requests working properly check failed" 1
fi

# [A-18] Are the POST requests working properly?
echo "Hello from Audit POST" > /tmp/audit_post.txt
post_upload=$(curl -s -i -F "file=@/tmp/audit_post.txt;filename=audit_uploaded.txt" http://127.0.0.1:8080/uploads/)
post_cgi=$(curl -s -d "hello_audit=world" http://127.0.0.1:8080/cgi-bin/hello.py)
if echo "$post_upload" | grep -q "201 Created" && echo "$post_cgi" | grep -q "hello_audit=world"; then
    record_result "A-18" "POST requests working properly (multipart upload -> 201, CGI POST -> 200)" 0
else
    record_result "A-18" "POST requests failed" 1
fi

# [A-19] Are the DELETE requests working properly?
del_status=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE http://127.0.0.1:8080/uploads/audit_uploaded.txt)
get_after_del=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/uploads/audit_uploaded.txt)
if [ "$del_status" = "204" ] && [ "$get_after_del" = "404" ]; then
    record_result "A-19" "DELETE requests working properly (204 on delete, 404 afterwards)" 0
else
    record_result "A-19" "DELETE requests failed (del: $del_status, get: $get_after_del)" 1
fi

# [A-20] Test a WRONG request, is the server still working properly?
garbage_resp=$(printf 'GARBAGE\r\n\r\n' | nc -w 2 -q 1 127.0.0.1 8080 2>/dev/null || true)
after_code=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/)
if echo "$garbage_resp" | grep -q "400 Bad Request" && [ "$after_code" = "200" ]; then
    record_result "A-20" "Wrong request handled (400 Bad Request, server continues serving 200 OK)" 0
else
    record_result "A-20" "Wrong request handling failed (garbage: '$garbage_resp', after: $after_code)" 1
fi

# [A-21] Upload some files to the server and get them back to test they were not corrupted.
dd if=/dev/urandom of=/tmp/audit_orig.bin bs=1024 count=20 2>/dev/null
orig_sha=$(sha256sum /tmp/audit_orig.bin | awk '{print $1}')
curl -s -F "file=@/tmp/audit_orig.bin;filename=audit_blob.bin" http://127.0.0.1:8080/uploads/ >/dev/null
curl -s http://127.0.0.1:8080/uploads/audit_blob.bin -o /tmp/audit_dl.bin
dl_sha=$(sha256sum /tmp/audit_dl.bin | awk '{print $1}')
curl -s -X DELETE http://127.0.0.1:8080/uploads/audit_blob.bin >/dev/null
if [ "$orig_sha" = "$dl_sha" ] && [ -n "$orig_sha" ]; then
    record_result "A-21" "Upload and roundtrip integrity check (20KB binary SHA256 matches: $orig_sha)" 0
else
    record_result "A-21" "Upload and roundtrip corrupted (orig: $orig_sha, dl: $dl_sha)" 1
fi

# [A-22] A working session and cookies system is present on the server?
rm -f /tmp/audit_cookie.txt
sess1=$(curl -s -c /tmp/audit_cookie.txt http://127.0.0.1:8080/session)
sess2=$(curl -s -b /tmp/audit_cookie.txt http://127.0.0.1:8080/session)
if echo "$sess1" | grep -q "Requests on it</th><td>1</td>" && \
   echo "$sess2" | grep -q "Requests on it</th><td>2</td>" && \
   grep -q "localserver_session" /tmp/audit_cookie.txt; then
    record_result "A-22" "Session and cookies system (Set-Cookie issued, visit counter increments 1 -> 2)" 0
else
    record_result "A-22" "Session and cookies system verification failed" 1
fi

# [A-23] Is the browser connecting with the server with no issues?
browser_status=$(curl -s -o /dev/null -w "%{http_code}" \
    -A "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36" \
    -H "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8" \
    -H "Accept-Language: en-US,en;q=0.5" \
    http://127.0.0.1:8080/)
if [ "$browser_status" = "200" ]; then
    record_result "A-23" "Browser connecting with server (full browser headers -> 200 OK)" 0
else
    record_result "A-23" "Browser connecting failed with status $browser_status" 1
fi

# [A-24] Are the request and response headers correct?
headers_full=$(curl -s -i http://127.0.0.1:8080/)
if echo "$headers_full" | grep -qi "^HTTP/1.1 200 OK" && \
   echo "$headers_full" | grep -qi "^Date:" && \
   echo "$headers_full" | grep -qi "^Server: localserver" && \
   echo "$headers_full" | grep -qi "^Content-Type: text/html" && \
   echo "$headers_full" | grep -qi "^Content-Length:" && \
   echo "$headers_full" | grep -qi "^Connection: keep-alive"; then
    record_result "A-24" "Response headers correct (HTTP/1.1, Date, Server, Content-Type, Content-Length, Connection)" 0
else
    record_result "A-24" "Response headers verification failed" 1
fi

# [A-25] Try a wrong URL on the server, is it handled properly?
wrong_url_code=$(curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/non_existent_page_path_12345)
if [ "$wrong_url_code" = "404" ]; then
    record_result "A-25" "Wrong URL handled properly (returns 404 Not Found)" 0
else
    record_result "A-25" "Wrong URL handled returned $wrong_url_code" 1
fi

# [A-26] Try to list a directory, is it handled properly?
listing_body=$(curl -s http://127.0.0.1:8080/listing/)
if echo "$listing_body" | grep -q "Index of /listing/" && \
   echo "$listing_body" | grep -q "data.csv" && \
   echo "$listing_body" | grep -q "nested/"; then
    record_result "A-26" "Directory listing handled properly (HTML autoindex listing entries & links)" 0
else
    record_result "A-26" "Directory listing check failed" 1
fi

# [A-27] Try a redirected URL, is it handled properly?
redir_headers=$(curl -s -i http://127.0.0.1:8080/old-page)
if echo "$redir_headers" | grep -q "301 Moved Permanently" && echo "$redir_headers" | grep -qi "Location: /index.html"; then
    record_result "A-27" "Redirected URL handled properly (301 Moved Permanently with Location: /index.html)" 0
else
    record_result "A-27" "Redirected URL check failed" 1
fi

# [A-28] Check the implemented CGI, does it work properly with chunked and unchunked data?
cgi_unchunked=$(curl -s -d "unchunked_payload=test" http://127.0.0.1:8080/cgi-bin/hello.py)
cgi_chunked=$(curl -s -H "Transfer-Encoding: chunked" -d "chunked_payload=test" http://127.0.0.1:8080/cgi-bin/hello.py)
cgi_perl=$(curl -s http://127.0.0.1:8080/cgi-bin/info.pl)
if echo "$cgi_unchunked" | grep -q "unchunked_payload=test" && \
   echo "$cgi_chunked" | grep -q "chunked_payload=test" && \
   echo "$cgi_perl" | grep -q "CGI is running (perl)"; then
    record_result "A-28" "CGI works properly with unchunked data, chunked data, and Perl script" 0
else
    record_result "A-28" "CGI chunked/unchunked test failed" 1
fi

echo "============================================================"
echo -e "Audit Summary: ${GREEN}$PASSED passed${RESET}, ${RED}$FAILED failed${RESET} of $TOTAL questions"
echo "============================================================"

if [ "$FAILED" -eq 0 ]; then
    exit 0
else
    exit 1
fi
