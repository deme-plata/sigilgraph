#!/usr/bin/env bash
# dp-lab.sh — isolated netns/netem lab for the delivery-law live measurement.
# 8 peer namespaces (dp1..dp8) on private bridge dpbr0 (10.99.99.0/24), each peer's
# veth degraded by netem IN BOTH DIRECTIONS. Production interfaces are never touched.
set -euo pipefail
N=${N:-8}
BR=dpbr0
SUBNET=10.99.99

cmd=${1:?setup|netem|clear-netem|teardown|status}
case "$cmd" in
  setup)
    ip link add $BR type bridge 2>/dev/null || true
    ip addr replace $SUBNET.1/24 dev $BR
    ip link set $BR up
    # defensive: allow lab traffic even if docker's FORWARD policy is DROP
    iptables -C FORWARD -s $SUBNET.0/24 -d $SUBNET.0/24 -j ACCEPT 2>/dev/null || \
      iptables -I FORWARD -s $SUBNET.0/24 -d $SUBNET.0/24 -j ACCEPT
    for i in $(seq 1 "$N"); do
      ip netns add dp$i 2>/dev/null || continue
      ip link add veth-dp$i type veth peer name eth0 netns dp$i
      ip link set veth-dp$i master $BR up
      ip -n dp$i addr add $SUBNET.$((10+i))/24 dev eth0
      ip -n dp$i link set eth0 up
      ip -n dp$i link set lo up
    done
    echo "lab up: $N peers on $SUBNET.11..$((10+N))"
    ;;
  netem)
    spec=${2:?netem spec, e.g. 'delay 20ms loss 25%'}
    for i in $(seq 1 "$N"); do
      # shellcheck disable=SC2086
      tc qdisc replace dev veth-dp$i root netem $spec
      # shellcheck disable=SC2086
      ip netns exec dp$i tc qdisc replace dev eth0 root netem $spec
    done
    echo "netem applied both directions on all $N peers: $spec"
    ;;
  clear-netem)
    for i in $(seq 1 "$N"); do
      tc qdisc del dev veth-dp$i root 2>/dev/null || true
      ip netns exec dp$i tc qdisc del dev eth0 root 2>/dev/null || true
    done
    echo "netem cleared"
    ;;
  status)
    for i in $(seq 1 "$N"); do
      echo "veth-dp$i: $(tc qdisc show dev veth-dp$i | head -1)"
    done
    ;;
  teardown)
    pkill -f "delivery-probe serve --name dp-lab" 2>/dev/null || true
    for i in $(seq 1 "$N"); do ip netns del dp$i 2>/dev/null || true; done
    ip link del $BR 2>/dev/null || true
    iptables -D FORWARD -s $SUBNET.0/24 -d $SUBNET.0/24 -j ACCEPT 2>/dev/null || true
    echo "lab down"
    ;;
esac
