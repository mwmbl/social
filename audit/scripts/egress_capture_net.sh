#!/usr/bin/env bash
# Capture every outbound connection a running Mwmbl Social container opens, for N minutes.
#
# NETWORK / WHAT IT CONTACTS:
#   - Docker Hub, once, to pull the nicolaka/netshoot image if it isn't already local.
#   - Nothing else of its own: tcpdump only listens inside the target container's network
#     namespace. The reverse-DNS step asks the container's own resolver (Docker's embedded DNS,
#     which forwards to your host's upstream resolver for non-Docker addresses) about each IP
#     that was seen.
# It does not start Mwmbl Social or Mastodon. Start the dev environment first (see below).
#
# Usage: egress_capture_net.sh <container> [minutes]
#   container  name or id of the running social container, e.g. mwmbl-social-e2e-social-1
#   minutes    how long to capture (default 10)
set -euo pipefail                                    # stop on errors and unset variables

container="${1:?usage: $0 <container> [minutes]}"   # the container to watch (required)
minutes="${2:-10}"                                   # capture length in minutes
out="$(mktemp -d)"                                   # scratch dir for the raw capture

# Tell the owner how to get the dev environment sending traffic (we never start it ourselves).
cat <<EOF
Before (or while) this runs, drive the dev setup from dev/mastodon/README.md, for example:
  cd dev/mastodon && ./setup.sh
  docker compose run --rm web bin/rails runner /seed/seed.rb
  docker compose up -d --build social
  docker compose exec social mwmbl-social register https://mastodon.test
  docker compose run --rm web bin/rails runner /seed/fasp.rb approve
  docker compose run --rm web bin/rails runner /seed/fasp.rb enable data_sharing
Then post, edit and delete as alice during the capture.
Capturing from ${container} for ${minutes} minute(s); raw log in ${out}/packets.txt
EOF

# The container's own addresses, so we keep only packets it sends (not ones sent to it).
own_ips="$(docker inspect -f \
  '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{.GlobalIPv6Address}} {{end}}' "$container")"
echo "Container addresses: ${own_ips}"

# tcpdump in a sidecar that shares the container's network namespace. The filter keeps
# IPv4 TCP SYNs without ACK (new connections), all UDP (DNS, QUIC) and all IPv6 TCP (BPF
# can't read IPv6 TCP flags, so IPv6 counts are packets, not connections).
# `timeout` ends it after the given minutes; `|| true` because timeout exits non-zero.
docker run --rm --net="container:${container}" --cap-add NET_RAW --cap-add NET_ADMIN \
  nicolaka/netshoot timeout "$((minutes * 60))" \
  tcpdump -n -l -i any '(tcp[tcpflags] & (tcp-syn|tcp-ack) == tcp-syn) or udp or (ip6 and tcp)' \
  > "${out}/packets.txt" 2>"${out}/tcpdump.err" || true

# Each tcpdump line looks like "... IP 172.18.0.5.43210 > 172.18.0.3.443: Flags [S], ...".
# Keep lines whose source address is one of ours, then print "dest-ip:port".
awk -v own="$own_ips" '
  BEGIN { n = split(own, a, " "); for (i = 1; i <= n; i++) mine[a[i]] = 1 }  # set of our IPs
  {
    for (i = 1; i <= NF; i++) if ($i == ">") break       # find the "src > dst:" arrow
    if (i > NF) next                                      # not a packet line
    src = $(i - 1); dst = $(i + 1); sub(/:$/, "", dst)    # strip the trailing colon
    sip = src; sub(/\.[^.]*$/, "", sip)                   # source address without its port
    port = dst; sub(/^.*\./, "", port)                    # last dot-separated field is the port
    if (!(sip in mine) && !(sip ~ /^127\./ && port == 53)) next  # only what we send (+ DNS on lo)
    ip = dst; sub(/\.[^.]*$/, "", ip)                     # everything before it is the address
    print ip ":" port
  }' "${out}/packets.txt" | sort | uniq -c | sort -rn > "${out}/destinations.txt"

# Reverse DNS for each unique address, in one more sidecar so we use the container's resolver.
echo; echo "count  destination            reverse DNS"
awk '{ sub(/:[0-9]+$/, "", $2); print $2 }' "${out}/destinations.txt" | sort -u > "${out}/ips.txt"  # unique IPs
docker run --rm -i --net="container:${container}" nicolaka/netshoot \
  sh -c 'while read ip; do echo "$ip $(dig +short +time=2 -x "$ip" | head -1)"; done' \
  < "${out}/ips.txt" > "${out}/rdns.txt" || true
# Join counts with names: rdns.txt has "ip name", destinations.txt has "count ip:port".
while read -r count dest; do                          # one line per unique destination
  name="$(awk -v ip="${dest%:*}" '$1 == ip { print $2 }' "${out}/rdns.txt")"   # its PTR name
  printf '%5s  %-22s %s\n' "$count" "$dest" "${name:--}"                      # the report line
done < "${out}/destinations.txt"
# DNS questions the container asked (tcpdump prints them as "A? host." or "AAAA? host.").
echo; echo "DNS lookups (count, type, name):"
grep -oE '(A|AAAA)\? [^ ]+' "${out}/packets.txt" | sort | uniq -c | sort -rn || echo "  none"
echo; echo "tcpdump messages: $(tail -n 2 "${out}/tcpdump.err" | tr '\n' ' ')"  # packet totals
