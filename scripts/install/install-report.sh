# Only fixed stage/outcome labels and exit codes are recorded. Never enable xtrace.
umask 077
stage=platform
seq=0
local_log=
upload_failed=0
report() {
    seq=$((seq + 1))
    body=$(printf '{"seq":%s,"stage":"%s","outcome":"%s","exit_code":%s}' "$seq" "$stage" "$1" "$2")
    if [ "$1" = failed ]; then
        kind=command_failed
        case "$2" in
            126) kind=permission_denied ;;
            127) kind=file_not_found ;;
            *) case "$stage" in download) kind=http_error ;; extract) kind=invalid_archive ;; service_start) kind=service_error ;; esac ;;
        esac
        body=$(printf '{"seq":%s,"stage":"%s","outcome":"failed","exit_code":%s,"error_kind":"%s"}' "$seq" "$stage" "$2" "$kind")
    fi
    if [ -n "$local_log" ]; then printf '%s\n' "$body" >> "$local_log" 2>/dev/null || true; fi
    if [ -z '@@KEY@@' ]; then upload_failed=1; return 0; fi
    escaped_body=$(printf '%s' "$body" | sed 's/"/\\"/g')
    # stdin config keeps the capability out of process arguments.
    curl -fsS --connect-timeout 2 --max-time 3 -K - @@URL@@ >/dev/null 2>&1 <<KANADE_REPORT_CONFIG || upload_failed=1
header = "X-Install-Key: @@KEY@@"
header = "Content-Type: application/json"
request = "POST"
data = "$escaped_body"
KANADE_REPORT_CONFIG
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -eq 0 ]; then rc=1; fi
    report failed "$rc"
    if [ -z "$local_log" ]; then printf '%s\n' "$body" >&2; fi
    echo "Installation attempt @@ID@@; safe local events: ${local_log:-unavailable}" >&2
    exit "$rc"
}
trap finish EXIT
if [ -z '@@KEY@@' ]; then echo 'Backend installation logging unavailable; diagnostics remain local.'; fi
echo "Installation attempt @@ID@@"
report progress 0
phase_lines=0
collect_stage() {
    [ -f "$TMP/install-stage" ] || return 0
    line=0
    while IFS= read -r next; do
        line=$((line + 1))
        [ "$line" -gt "$phase_lines" ] || continue
        case "$next" in
            placement|service_start) stage=$next; report progress 0 ;;
        esac
        phase_lines=$line
    done < "$TMP/install-stage"
}
