#!/usr/bin/env bash
# Boots a stock Ubuntu cloud image under QEMU/KVM with a veneer NoCloud seed ISO and waits
# for the workspace-ready marker that the generated first-boot script prints on the console.
# Usage: boot-cloudinit.sh DIR   (DIR holds ubuntu.img and seed.iso)
set -u
dir="$1"
img=/var/tmp/ci-root.qcow2
rm -f "$img" /var/tmp/ci-console.log
qemu-img create -q -f qcow2 -F qcow2 -b "$dir/ubuntu.img" "$img" 8G
if [ "$(uname -m)" = aarch64 ]; then
  cp /usr/share/qemu-efi-aarch64/QEMU_EFI.fd /var/tmp/ci-efi.fd && truncate -s 64M /var/tmp/ci-efi.fd
  sudo timeout 600 qemu-system-aarch64 -M virt -accel kvm -cpu host -m 2048 -smp 2 -nographic \
    -drive if=pflash,format=raw,file=/var/tmp/ci-efi.fd,readonly=on \
    -drive file="$img",format=qcow2,if=virtio \
    -drive file="$dir/seed.iso",format=raw,if=virtio,media=cdrom \
    -nic user,model=virtio-net-pci >/var/tmp/ci-console.log 2>&1 &
else
  sudo timeout 600 qemu-system-x86_64 -accel kvm -cpu host -m 2048 -smp 2 -nographic \
    -drive file="$img",format=qcow2,if=virtio \
    -drive file="$dir/seed.iso",format=raw,if=virtio,media=cdrom \
    -nic user,model=virtio-net-pci >/var/tmp/ci-console.log 2>&1 &
fi
qpid=$!
ok=1
for _ in $(seq 1 580); do
  if grep -aq "veneer: workspace ready" /var/tmp/ci-console.log; then ok=0; break; fi
  if grep -aq "veneer: workspace failed" /var/tmp/ci-console.log; then break; fi
  sleep 1
done
sudo kill "$qpid" 2>/dev/null; sleep 2
[ $ok = 0 ] || { echo "no ready marker"; tail -c 3000 /var/tmp/ci-console.log; }
exit $ok
