# Helm

**Helm** switches the operating system that the machine boots into.

It finds the systems that are installed on the disks of the machine, shows
them as cards, and points the firmware at the one the user picks. The switch
happens through the UEFI one shot variable `BootNext`, so the usual boot order
is never touched: if the new system does not come up, the firmware simply
falls back to it.

```
┌──────────────────┐   Unix socket / named pipe   ┌────────────────────┐
│  Helm (GUI)      │  ─────────────────────────▶  │  helm-helper       │
│  Tauri, no root  │   length prefixed JSON       │  runs as root      │
└──────────────────┘                              └─────────┬──────────┘
                                                            │
                                    ┌───────────────────────┴──────────────┐
                                    │ raw block devices, GPT, filesystems  │
                                    │ /sys/firmware/efi/efivars (Linux)    │
                                    │ GetFirmwareEnvironmentVariable (Win) │
                                    └──────────────────────────────────────┘
```

## The pieces

| Crate | What it does |
| --- | --- |
| `crates/helm-protocol` | The data types and the framing of the protocol between the two processes. |
| `crates/helm-detect` | Enumerates the disks, parses the partition tables and looks for the installed systems. |
| `crates/helm-boot` | Reads and writes the UEFI firmware variables, builds device paths, reboots the machine. |
| `crates/helm-ipc` | The local transport: a Unix domain socket on Linux, a named pipe on Windows. |
| `crates/helm-helper` | The privileged helper: a small service that does the work that needs root rights. |
| `crates/helm-gui` | The interface: a Tauri application that never needs any privilege. |
| `ui/` | The interface itself: plain HTML, CSS and JavaScript, no build step. |

The interface and the helper are two separate processes on purpose: the
interface runs with the rights of the user, the helper runs as root (as a
systemd service on Linux, as a Windows service on Windows) and is the only
process that touches the disks and the firmware.

## How the systems are found

Helm never uses the "my drives" API of the operating system, because those
only ever report the disks of the system that is running. Instead:

1. **The disks are enumerated.** On Linux the whole disks are found in
   `/sys/block`, on Windows through `\\.\PhysicalDriveN` and
   `IOCTL_STORAGE_QUERY_PROPERTY`.
2. **The partition table is parsed from the raw device.** The protective MBR,
   the GPT header and the partition entries are read directly, both the
   checksum of the header and the checksum of the entry array are verified.
3. **The filesystem of every partition is recognised** by its superblock:
   NTFS, ext2/3/4, FAT12/16/32, XFS and btrfs.
4. **The filesystems are inspected without being mounted.** Helm contains its
   own readers for NTFS, ext4 and FAT, so it can look inside a Windows
   installation or a Linux root filesystem while it is not running from it.
5. **The operating systems are recognised by the files that prove they can
   boot**, the same way `os-prober` and rEFInd do it:
   * *Windows*: a NTFS partition with `\Windows\System32\winload.efi` and a
     BCD store, or a firmware entry that points at `bootmgfw.efi`.
   * *Linux*: an ext4/btrfs/xfs partition with `/etc/os-release`,
     `/boot/vmlinuz*`, `/boot/initramfs*` and either a GRUB configuration or
     a systemd-boot entry. The name of the distribution comes from
     `/etc/os-release`.
6. **The boot loaders are looked up on the EFI System Partitions.** The `EFI`
   directory of every FAT32 partition is listed, and the loaders it holds
   (shim, GRUB, systemd-boot, `bootmgfw.efi`) are matched with the systems
   that were found.

Nothing is ever mounted and nothing is ever written to a disk.

## How the boot is switched

The firmware is told about the new target with the one shot variable
`BootNext`:

* When the firmware already knows the loader (there is a `Boot####` variable
  that points at it), the **raw bytes of that variable are copied** into
  `BootNext`. This is the safest route: the firmware gets exactly the entry
  it created itself.
* When there is no such entry, Helm **builds one**: an `HD()` node for the
  EFI System Partition (with the partition GUID, the number and the size) and
  a `File()` node for the loader, followed by the end node.

The variable is written through `efivarfs` on Linux and through
`SetFirmwareEnvironmentVariableW` on Windows. Both need root rights, which is
why they live in the helper.

## Building

```sh
cargo build --release
```

The interface needs a web view: on Linux `libwebkit2gtk-4.1` and `libgtk-3`
have to be installed.

## Installing on Linux

```sh
cargo build --release
sudo ./packaging/linux/install.sh
```

The script installs the interface and the helper, registers the helper as a
systemd service (`packaging/linux/helm-helper.service`) and adds the user to
the `helm` group, which is the group that may talk to the helper.

```sh
helm-gui                                   # start the interface
systemctl status helm-helper               # the helper
journalctl -u helm-helper -f               # its log
```

## Installing on Windows

Build the bundles with the Tauri bundler:

```sh
cargo tauri build
```

The MSI installs the interface and the helper. The helper is registered as a
Windows service, either with

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\install-service.ps1
```

or with the helper itself:

```powershell
helm-helper --install-service
```

The helper runs as the local system account. The named pipe accepts the
administrators, the system account and the authenticated users, but a client
that is not an administrator may only ask for a detection: the requests that
write a firmware variable or reboot the machine are refused.

## Testing

```sh
cargo test --workspace
```

The test suite covers

* the GPT parser, including the checksums and the broken tables,
* the UEFI device paths and the `LOAD_OPTION` decoding, against bytes that
  were captured from a real firmware,
* the filesystems, against **real** ext4, FAT32 and NTFS images that are
  built by the test (the NTFS fixture is stored compressed in the repository),
* the whole detection pipeline, against a disk image with a GPT that holds a
  Linux partition, a Windows partition and an EFI System Partition,
* the protocol between the interface and the helper, over a real socket.

The tests that need the firmware variables of the machine are skipped when
the machine did not boot through UEFI.

## Trying it without touching the machine

The helper can look at a disk image instead of the real disks:

```sh
helm-helper --detect --disk /path/to/disk.img    # print the report as JSON
helm-helper --serve --disk /path/to/disk.img     # serve the interface
```

The interface finds the helper through `HELM_HELPER_SOCKET`, which is handy
while developing:

```sh
HELM_HELPER_SOCKET=/tmp/helm.sock helm-helper --serve &
HELM_HELPER_SOCKET=/tmp/helm.sock helm-gui
```

## Known limitations

* Only UEFI is supported. A machine that was started through the legacy BIOS
  has no firmware variables to write, and Helm says so in the interface.
* The ext4 reader does not support `inline_data`, `meta_bg`, encryption or
  compression. A file that is stored inline cannot be read; the rest of the
  filesystem still is.
* XFS is recognised, but its content is not read. A Linux installation that lives
  on XFS is therefore only found when the firmware or the EFI System Partition
  point at its loader.
* macOS is not detected. The APFS container is a much bigger job than the
  other filesystems.
