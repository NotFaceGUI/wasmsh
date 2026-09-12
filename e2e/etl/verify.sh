#!/bin/sh
# Independent invariant check for the ETL output. Recomputes the core
# invariants with a DIFFERENT algorithm than run.sh and fails on any
# discrepancy. Non-vacuous: checks total preservation, non-empty denominators,
# cross-table consistency, and manifest integrity.
set -euo pipefail

fail=0
note() { echo "VERIFY: $*"; }
bad() { echo "VERIFY-FAIL: $*" >&2; fail=1; }

# ---- 1. summary.json must parse and expose required keys ---------------
jq -e '.orders.total and .orders.matched and .regions and .log.lines' \
  /out/summary.json >/dev/null || { bad "summary.json missing keys"; }

total=$(jq -r '.orders.total' /out/summary.json)
matched=$(jq -r '.orders.matched' /out/summary.json)
mismatch=$(jq -r '.orders.mismatch' /out/summary.json)
missing=$(jq -r '.orders.missing' /out/summary.json)
extra=$(jq -r '.orders.extra' /out/summary.json)
rejected=$(jq -r '.orders.rejected' /out/summary.json)

# ---- 2. Non-vacuous denominators ----------------------------------------
# A pipeline that silently produced nothing must fail, not pass "0 == 0".
if [ "$total" -le 0 ]; then bad "orders.total is $total"; fi
if [ "$matched" -le 0 ]; then bad "orders.matched is $matched"; fi
if [ "$(jq -r '.regions | length' /out/summary.json)" -le 0 ]; then
  bad "no regions aggregated"
fi
if [ "$(jq -r '.log.lines' /out/summary.json)" -le 0 ]; then
  bad "no log lines counted"
fi

# ---- 3. Cross-table conservation (independent recount) ------------------
# Every accepted order is exactly one of matched / mismatch / missing.
accepted=$(wc -l < /out/orders.tsv)
recount_matched=$(wc -l < /out/matched.tsv)
recount_mismatch=$(wc -l < /out/mismatch.tsv)
recount_missing=$(wc -l < /out/missing.tsv)
classified=$((recount_matched + recount_mismatch + recount_missing))
if [ "$classified" -ne "$accepted" ]; then
  bad "classification sum $classified != accepted $accepted"
fi
if [ "$recount_matched" -ne "$matched" ]; then
  bad "matched recount $recount_matched != summary $matched"
fi
if [ "$recount_mismatch" -ne "$mismatch" ]; then
  bad "mismatch recount $recount_mismatch != summary $mismatch"
fi
if [ "$recount_missing" -ne "$missing" ]; then
  bad "missing recount $recount_missing != summary $missing"
fi
if [ "$accepted" -ne "$total" ]; then
  bad "accepted $accepted != summary total $total"
fi

# ---- 4. Match set is exactly the amount-equal orders --------------------
# Independently join the accepted orders (order_id -> amount) with events
# using a sorted line-up, not the hash-order iteration run.sh uses.
cut -f1,4 /out/orders.tsv | sort -t$'\t' -k1,1 > /tmp/_v_orders
sort -t$'\t' -k1,1 /out/events.tsv > /tmp/_v_events
awk -F'\t' '
  FNR == NR { e[$1] = $2; next }
  { if (($1 in e) && (e[$1] == $2)) n++ }
  END { print n + 0 }
' /tmp/_v_events /tmp/_v_orders > /tmp/_v_matched
v_matched=$(cat /tmp/_v_matched)
if [ "$v_matched" -ne "$matched" ]; then
  bad "independent matched count $v_matched != summary $matched"
fi

# Every mismatch row must have differing amounts.
awk -F'\t' '{ if ($2 == $3) { print "bad mismatch " $0; exit 1 } }' /out/mismatch.tsv >/dev/null \
  || bad "mismatch.tsv contains equal amounts"

# ---- 5. Region totals must equal a fresh independent sum ----------------
# Independent route: map order_id -> region from orders.tsv, then sum matched
# amounts per region, and compare to region_totals.tsv.
awk -F'\t' '
  FNR == NR { region[$1] = $3; next }
  { s[region[$1]] += $2 }
  END { for (r in s) printf "%s\t%d\n", r, s[r] }
' /out/orders.tsv /out/matched.tsv | sort -t$'\t' -k1,1 > /tmp/_v_region_expected
if ! cmp -s /tmp/_v_region_expected /out/region_totals.tsv; then
  bad "region_totals.tsv disagrees with independent recomputation"
fi
regions_impl=$(jq -r '.regions | length' /out/summary.json)
regions_recount=$(grep -c . /tmp/_v_region_expected)
if [ "$regions_impl" -ne "$regions_recount" ]; then
  bad "region count $regions_impl != recount $regions_recount"
fi
if [ "$regions_recount" -le 0 ]; then
  bad "no regions computed"
fi

# ---- 6. Rejected trace must be non-vacuous and consistent --------------
rejected_rows=$(( $(wc -l < /out/rejected.tsv) - 1 ))
if [ "$rejected_rows" -ne "$rejected" ]; then
  bad "rejected trace rows $rejected_rows != summary $rejected"
fi
if [ "$rejected_rows" -le 0 ]; then
  bad "no rows were rejected; the dirty data went undetected"
fi

# ---- 7. MANIFEST integrity ---------------------------------------------
if ! ( cd /pkg && sha256sum -c MANIFEST >/dev/null ); then
  bad "MANIFEST verification failed"
fi
manifest_count=$(grep -c . /pkg/MANIFEST)
if [ "$manifest_count" -le 0 ]; then bad "empty MANIFEST"; fi

# ---- 8. Archive round-trip: extract and re-verify ----------------------
rm -rf /verify_extract
mkdir -p /verify_extract
tar -xf /out/publish.tar -C /verify_extract
if ! ( cd /verify_extract && sha256sum -c MANIFEST >/dev/null ); then
  bad "extracted archive contents fail MANIFEST"
fi
# Compare extracted payload bytes against /pkg (archive bytes themselves are
# not compared, since tar embeds mtimes).
for f in /pkg/*; do
  base=$(basename "$f")
  [ "$base" = MANIFEST ] && continue
  if ! cmp -s "$f" "/verify_extract/$base"; then
    bad "archive round-trip differs for $base"
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "VERIFY-FAILED" >&2
  exit 1
fi
note "all invariants hold"
