# Full restore onto a new machine

`adapters/ubuntu-os/restore_system.py` restores an Upkeep snapshot onto an empty disk, so that the disk boots the restored Ubuntu system on its own. Use it to replace a lost server, or to move a server to new hardware or a new cloud instance.

## What you need

- A new machine with an empty disk, at least as large as the data in the snapshot.
- A rescue or live Linux on that machine, with `python3`, `sfdisk`, `mkfs.ext4`, `mkfs.vfat`, `wipefs`, `blkid` and network access to the destination. An Ubuntu live ISO, a cloud rescue mode, or the cloud image booted from a second disk all work.
- From the client machine: the `adapters` folder of this repository, the restic binary (`packaging/install-restic.sh <dir>`), the destination's repository key (`~/.config/upkeep/secrets/<dest>.repository-key`) and the SSH key or password of the destination.
- The destination's SSH host keys, as in `~/.config/upkeep/destinations.toml`.
- The snapshot id: `upkeep snapshots <host> --dest <dest>`. Default: the latest snapshot.

## Steps

1. Boot the new machine into the rescue system. Find the empty disk with `lsblk`.
2. Copy the files to the rescue system:

   ```sh
   scp -r adapters restic ~/.config/upkeep/secrets/laptop.repository-key ~/.ssh/upkeep_dest root@rescue:/root/kit/
   ```

3. Run the restore as root:

   ```sh
   python3 /root/kit/adapters/ubuntu-os/restore_system.py \
     --disk /dev/nvme1n1 \
     --ssh backup@100.64.0.21 --repository /backups/upkeep/erp-aws \
     --host-key "ssh-ed25519 AAAAC3Nza..." \
     --key-file /root/kit/upkeep_dest \
     --repository-key-file /root/kit/laptop.repository-key \
     --engine /root/kit/restic \
     --snapshot 3612fbdc
   ```

   Use `--password-file <file>` instead of `--key-file` for a destination with password auth. The script asks you to type the disk name before it erases the disk; `--yes` skips the question.
4. Remove the rescue system or the rescue disk and boot from the restored disk.
5. Check the services. Then remove the kit from the rescue system.

## What the script does

| Step | Detail |
|---|---|
| Check | Refuses a device that is not a whole disk or that has mounted filesystems. Reads the snapshot over SFTP with strict host key checks. |
| Partition | GPT with a 1 MiB BIOS boot partition, a 512 MiB EFI system partition (`--esp-size`) and an ext4 root filesystem on the rest of the disk. The disk boots with BIOS and UEFI firmware. |
| Restore | Mounts the root filesystem and the EFI partition at `/boot/efi`, then restores the whole snapshot into it. A separate `/boot` of the old system is now a folder on the root filesystem. |
| fstab | `/` and `/boot/efi` get the new filesystem UUIDs. The `/boot` and swap lines are commented out. Other mounts, such as data disks, get `nofail`, so a missing disk does not stop the boot. |
| Machine id | Kept. With `--new-machine-id`, `/etc/machine-id` is emptied and systemd makes a new one at first boot. Use it when the old server stays in service. |
| Network | `--network dhcp` (default): the old netplan files are renamed to `*.yaml.upkeep-restore`, a new netplan file sets DHCP on every `en*` and `eth*` interface, and cloud-init network configuration is turned off. Old configs often match the old MAC address. `--network keep` changes nothing. |
| Boot loader | Comments out `GRUB_FORCE_PARTUUID` of cloud images, installs GRUB for BIOS and for UEFI (`--removable`, no NVRAM change), and runs `update-initramfs` and `update-grub` in the restored system. |

## After the restore

- SSH host keys, users, hostname and services are those of the snapshot.
- The snapshot was taken while its job ran. The runner on the restored system marks that job's running step as interrupted and runs the job's `on_failure` steps, for example `writers:maintenance_off`.
- Swap is not restored. Make a new swap file if the server needs one.
- To use a static address again, move the old netplan file back and delete `/etc/netplan/90-upkeep-restore.yaml`, then run `netplan apply`.
