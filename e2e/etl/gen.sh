#!/bin/sh
# Deterministic input generation for the offline ETL acceptance project.
#
# Everything here is a pure function of the loop counters (no $RANDOM, no
# wall clock), so re-running produces byte-identical inputs.
set -euo pipefail

mkdir -p /in /out
rm -f /in/access.log /in/orders.csv /in/users.csv /in/events.jsonl

# ---- nginx access log: 240 lines, 6 fixed status codes -------------------
awk 'BEGIN{
  split("10.0.0 10.0.1 10.0.2 10.0.3", nets, " ");
  split("/api/orders /api/users /health /static/app.js /api/items", paths, " ");
  split("200 200 200 404 500 301", codes, " ");
  for (i = 1; i <= 240; i++) {
    ip = nets[(i % 4) + 1] "." (i % 251);
    p = paths[(i % 5) + 1];
    code = codes[(i % 6) + 1];
    bytes = 100 + (i * 37) % 900;
    ts = 1700000000 + i * 60;
    printf "%s - - [%d] \"GET %s HTTP/1.1\" %s %d\n", ip, ts, p, code, bytes;
  }
}' > /in/access.log

# ---- users: user_id -> region -------------------------------------------
cat > /in/users.csv <<'CSV'
user_id,region
u1,us-east
u2,us-west
u3,eu-west
u4,ap-south
u5,us-east
u6,eu-west
CSV

# ---- orders: valid rows plus deliberate dirty rows ----------------------
cat > /in/orders.csv <<'CSV'
order_id,user_id,region,amount
o1,u1,us-east,100
o2,u2,us-west,250
o3,u3,eu-west,75
o4,u4,ap-south,300
o5,u5,us-east,120
o6,u6,eu-west,90
o7,u1,us-east,50
o8,u2,us-west,
o9,u3,eu-west,notanumber
o10,u4,ap-south,400
o7,u1,us-east,50
o11,u5,us-east,60
CSV

# ---- events: JSONL with one mismatch and one missing order --------------
cat > /in/events.jsonl <<'JSONL'
{"order_id":"o1","amount":100}
{"order_id":"o2","amount":250}
{"order_id":"o3","amount":75}
{"order_id":"o4","amount":300}
{"order_id":"o5","amount":999}
{"order_id":"o7","amount":50}
{"order_id":"o10","amount":400}
{"order_id":"o11","amount":60}
{"order_id":"o99","amount":10}
JSONL

echo "generated: $(wc -l < /in/access.log) log lines"
