#!/usr/bin/env bash
# Validates that CPU isolation kernel boot parameters are active.
#
# To enable, add to GRUB_CMDLINE_LINUX in /etc/default/grub:
#   isolcpus=2,3 nohz_full=2,3 rcu_nocbs=2,3
# Then: sudo update-grub && sudo reboot
#
# These settings ensure the matching benchmark thread is not interrupted
# by the OS scheduler, timer ticks, or RCU callbacks.

set -euo pipefail

ISOLATED=$(cat /sys/devices/system/cpu/isolated 2>/dev/null || echo "")
NOHZ=$(cat /sys/devices/system/cpu/nohz_full 2>/dev/null || echo "")

echo "=== CPU Isolation Status ==="
echo "Isolated CPUs : ${ISOLATED:-none}"
echo "nohz_full CPUs: ${NOHZ:-none}"

if [[ -z "$ISOLATED" ]]; then
    echo ""
    echo "WARNING: No CPUs are isolated. Latency benchmarks will show OS jitter."
    echo ""
    echo "For sub-µs p99, add to kernel cmdline:"
    echo "  isolcpus=2,3 nohz_full=2,3 rcu_nocbs=2,3"
    echo ""
    echo "Steps:"
    echo "  1. Edit /etc/default/grub"
    echo "  2. Add the above to GRUB_CMDLINE_LINUX"
    echo "  3. Run: sudo update-grub && sudo reboot"
    echo "  4. Re-run this script to verify"
    exit 1
fi

echo ""
echo "CPU isolation active on: $ISOLATED"
echo "Ready for RT benchmarks. Run: sudo bash scripts/run-bench-rt.sh"
