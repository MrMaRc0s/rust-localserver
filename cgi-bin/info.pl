#!/usr/bin/env perl
# A second CGI system, in a different language, registered under .pl in the
# same route. Nothing in the server knows about Perl: the interpreter is a
# config value keyed by file extension, so this is a config change only.
use strict;
use warnings;

my $body = '';
if (defined $ENV{CONTENT_LENGTH} && $ENV{CONTENT_LENGTH} > 0) {
    read(STDIN, $body, $ENV{CONTENT_LENGTH});
}

sub esc {
    my $text = shift // '';
    $text =~ s/&/&amp;/g; $text =~ s/</&lt;/g; $text =~ s/>/&gt;/g;
    return $text;
}

print "Content-Type: text/html; charset=utf-8\r\n";
print "X-Cgi-Language: perl\r\n";
print "\r\n";
print "<!doctype html><html><head><meta charset='utf-8'><title>CGI - perl</title>";
print "<style>body{margin:0;padding:2rem 1.5rem;background:#10151c;color:#e6edf3;";
print "font:15px/1.6 system-ui,sans-serif}main{max-width:44rem;margin:0 auto}";
print "table{width:100%;border-collapse:collapse}th,td{text-align:left;padding:.35rem .6rem;";
print "border-bottom:1px solid #2a3441}th{color:#8b98a5;width:12rem}";
print "code{font-family:ui-monospace,Menlo,monospace}a{color:#58a6ff}</style>";
print "</head><body><main><h1>CGI is running (perl)</h1>";
print "<p>Perl $] &mdash; the server does not know this script is Perl. The ";
print "interpreter is chosen by the <code>.pl</code> entry in the route's ";
print "<code>cgi</code> map.</p><table>";
for my $name (qw(REQUEST_METHOD SCRIPT_NAME SCRIPT_FILENAME PATH_INFO
                 QUERY_STRING SERVER_PROTOCOL GATEWAY_INTERFACE REMOTE_ADDR)) {
    printf "<tr><th>%s</th><td><code>%s</code></td></tr>\n", $name, esc($ENV{$name});
}
print "</table>";
printf "<p>Body: %d bytes <code>%s</code></p>", length($body), esc($body);
print "<p><a href='/'>Back to the index</a></p></main></body></html>\n";
