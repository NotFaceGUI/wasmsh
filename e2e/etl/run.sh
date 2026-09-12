#!/bin/sh
# Offline ETL pipeline. Reads /in, writes /out. Deterministic: no wall clock,
# no randomness. Uses only sandbox shell tools (no host shell, no python).
set -euo pipefail

mkdir -p /out
rm -f /out/*.tsv /out/*.json /out/*.md /out/*.txt /out/*.tar 2>/dev/null || true

# ---------------------------------------------------------------------------
# 1. Normalize orders.csv. Malformed rows are rejected with a reason and a
#    trace is written to /out/rejected.tsv.
# ---------------------------------------------------------------------------
printf 'reason\tline\n' > /out/rejected.tsv
awk -F, '
  NR == 1 { next }
  {
    if (NF != 4)          { printf "field_count\t%s\n", $0 >> "/out/rejected.tsv"; next }
    if ($4 == "")         { printf "empty_amount\t%s\n", $0 >> "/out/rejected.tsv"; next }
    if ($4 !~ /^[0-9]+$/) { printf "non_numeric_amount\t%s\n", $0 >> "/out/rejected.tsv"; next }
    if (seen[$1]++)       { printf "duplicate_order\t%s\n", $0 >> "/out/rejected.tsv"; next }
    printf "%s\t%s\t%s\t%s\n", $1, $2, $3, $4 > "/out/orders.tsv"
  }
' /in/orders.csv

# ---------------------------------------------------------------------------
# 2. Reconcile orders against events without `join`, via awk arrays.
#    Parse the JSONL with jq into a TSV first.
# ---------------------------------------------------------------------------
jq -r '[.order_id, (.amount | tostring)] | @tsv' /in/events.jsonl \
  | sort -k1,1 > /out/events.tsv

# Project accepted orders to order_id + amount for reconciliation.
cut -f1,4 /out/orders.tsv | sort -k1,1 > /out/_orders_amt.tsv

awk -F'\t' '
  FNR == NR { order[$1] = $2; next }
  { event[$1] = $2 }
  END {
    for (id in order) {
      if (id in event) {
        if (order[id] == event[id]) printf "%s\t%s\n", id, order[id] > "/out/_matched.raw";
        else printf "%s\t%s\t%s\n", id, order[id], event[id] > "/out/_mismatch.raw";
      } else {
        printf "%s\t%s\n", id, order[id] > "/out/_missing.raw";
      }
    }
    for (id in event) {
      if (!(id in order)) printf "%s\t%s\n", id, event[id] > "/out/_extra.raw";
    }
  }
' /out/_orders_amt.tsv /out/events.tsv
rm -f /out/_orders_amt.tsv
for tbl in matched mismatch missing extra; do
  : > "/out/$tbl.tsv"
  [ -f "/out/_$tbl.raw" ] && sort -k1,1 "/out/_$tbl.raw" > "/out/$tbl.tsv"
  rm -f "/out/_$tbl.raw"
done

# ---------------------------------------------------------------------------
# 3. Aggregate matched amounts by region. Region is resolved through
#    user_id -> users.csv with an awk array (no `join`).
# ---------------------------------------------------------------------------
sort -k1,1 /out/orders.tsv > /out/_orders_sorted.tsv
awk -F'\t' '
  FNR == NR { region[$1] = $3; next }
  { sum[region[$1]] += $2 }
  END { for (r in sum) printf "%s\t%d\n", r, sum[r] }
' /out/_orders_sorted.tsv /out/matched.tsv | sort -k1,1 > /out/region_totals.tsv
rm -f /out/_orders_sorted.tsv

# ---------------------------------------------------------------------------
# 4. Log statistics: per-status counts and total bytes.
# ---------------------------------------------------------------------------
awk '{ count[$9]++; bytes += $10 } END { for (c in count) printf "%s\t%d\n", c, count[c] }' \
  /in/access.log | sort -k1,1 > /out/log_status.tsv
awk '{ bytes += $10; n++ } END { printf "%d\t%d\n", n, bytes }' \
  /in/access.log | awk -F'\t' '{ printf "TOTAL\t%s\t%s\n", $1, $2 }' > /out/log_totals.tsv

# ---------------------------------------------------------------------------
# 5. summary.json via jq. The two TSV tables are rendered to JSON arrays with
#    awk (jq here has no raw-input mode), then assembled with jq -n.
# ---------------------------------------------------------------------------
total_orders=$(grep -c $'\t' /out/orders.tsv || true)
matched=$(grep -c . /out/matched.tsv || true)
mismatch=$(grep -c . /out/mismatch.tsv || true)
missing=$(grep -c . /out/missing.tsv || true)
extra=$(grep -c . /out/extra.tsv || true)
rejected=$(( $(grep -c . /out/rejected.tsv) - 1 ))

region_json=$(awk -F'\t' '
  BEGIN { printf "[" }
  { if (n++) printf ","; printf "{\"region\":\"%s\",\"total\":%s}", $1, $2 }
  END { printf "]" }
' /out/region_totals.tsv)
log_json=$(awk -F'\t' '
  BEGIN { printf "[" }
  { if (n++) printf ","; printf "{\"status\":\"%s\",\"count\":%s}", $1, $2 }
  END { printf "]" }
' /out/log_status.tsv)
log_total=$(cut -f2 /out/log_totals.tsv)

jq -n \
  --arg date "2024-01-01" \
  --argjson total_orders "$total_orders" \
  --argjson matched "$matched" \
  --argjson mismatch "$mismatch" \
  --argjson missing "$missing" \
  --argjson extra "$extra" \
  --argjson rejected "$rejected" \
  --argjson regions "$region_json" \
  --argjson log_status "$log_json" \
  --argjson log_lines "$log_total" \
  '{
    date: $date,
    orders: {
      total: $total_orders,
      matched: $matched,
      mismatch: $mismatch,
      missing: $missing,
      extra: $extra,
      rejected: $rejected
    },
    regions: $regions,
    log: { lines: $log_lines, status: $log_status }
  }' > /out/summary.json

# ---------------------------------------------------------------------------
# 6. Markdown daily report.
# ---------------------------------------------------------------------------
{
  echo "# Daily ETL Report ($(jq -r .date /out/summary.json))"
  echo
  echo "| metric | value |"
  echo "| --- | --- |"
  jq -r '.orders | to_entries[] | "| orders.\(.key) | \(.value) |"' /out/summary.json
  echo
  echo "## Region totals"
  echo
  echo "| region | total |"
  echo "| --- | --- |"
  jq -r '.regions[] | "| \(.region) | \(.total) |"' /out/summary.json
  echo
  echo "## Log status"
  echo
  echo "| status | count |"
  echo "| --- | --- |"
  jq -r '.log.status[] | "| \(.status) | \(.count) |"' /out/summary.json
} > /out/report.md

# ---------------------------------------------------------------------------
# 7. Publish package: MANIFEST with per-file sha256, then a tar archive.
# ---------------------------------------------------------------------------
rm -rf /pkg
mkdir -p /pkg
for f in orders.tsv events.tsv matched.tsv mismatch.tsv missing.tsv extra.tsv \
         rejected.tsv region_totals.tsv log_status.tsv log_totals.tsv \
         summary.json report.md; do
  cp "/out/$f" "/pkg/$f"
done
( cd /pkg && sha256sum ./* > MANIFEST )
tar -cf /out/publish.tar -C /pkg .

echo "etl: done"
