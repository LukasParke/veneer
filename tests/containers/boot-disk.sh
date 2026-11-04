#!/usr/bin/env bash
# Boots a veneer disk image under QEMU/KVM (arm64 UEFI), waits for the workspace phase on
# the serial console, then checks the workspace on the disk itself. Usage: boot-disk.sh DISK
set -u
disk="$1"
cp --sparse=always "$disk" /var/tmp/vm.raw
if [ "$(uname -m)" = aarch64 ]; then
  # arm64: d2vm uses grub-efi -> boot with UEFI firmware.
  cp /usr/share/qemu-efi-aarch64/QEMU_EFI.fd /var/tmp/efi.fd && truncate -s 64M /var/tmp/efi.fd
  sudo timeout 150 qemu-system-aarch64 -M virt -accel kvm -cpu host -m 1024 -nographic \
    -drive if=pflash,format=raw,file=/var/tmp/efi.fd,readonly=on \
    -drive file=/var/tmp/vm.raw,format=raw,if=virtio >/var/tmp/vm-console.log 2>&1 &
else
  # amd64: d2vm uses syslinux (BIOS) and console=ttyS0.
  sudo timeout 150 qemu-system-x86_64 -accel kvm -cpu host -m 1024 -nographic \
    -drive file=/var/tmp/vm.raw,format=raw,if=virtio >/var/tmp/vm-console.log 2>&1 &
fi
qpid=$!
for _ in $(seq 1 140); do grep -aq "ready: default" /var/tmp/vm-console.log && break; sleep 1; done
sudo kill "$qpid" 2>/dev/null; sleep 2
grep -aq "ready: default" /var/tmp/vm-console.log || { echo "no workspace on console"; tail -c 2000 /var/tmp/vm-console.log; exit 1; }
d=$(sudo losetup -fP --show /var/tmp/vm.raw)
sudo mount "${d}p2" /mnt
sudo grep -q "hello from alpine microvm" /mnt/work/default/hello.txt; rc=$?
sudo umount /mnt; sudo losetup -d "$d"
exit $rc
