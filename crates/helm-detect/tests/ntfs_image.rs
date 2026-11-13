//! Integration test for the NTFS reader.
//!
//! The fixture is a small NTFS volume that holds a Windows installation
//! layout (`\Windows\System32\winload.efi`, `\Windows\System32\ntoskrnl.exe`
//! and a `\Boot\BCD` store). It is stored compressed in the repository.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use helm_detect::block::{BlockDevice, FileDevice, PartitionDevice};
use helm_detect::fs::{detect_filesystem, open_filesystem};
use helm_detect::os::windows;
use helm_protocol::FileSystemKind;

/// Unpack the compressed fixture.
fn fixture() -> PathBuf {
    let path = std::env::temp_dir().join(format!("helm-ntfs-{}.img", std::process::id()));
    let compressed = std::fs::File::open(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ntfs-windows.img.gz"),
    )
    .expect("open the fixture");
    let mut decoder = flate2::read::GzDecoder::new(compressed);
    let mut data = Vec::new();
    decoder.read_to_end(&mut data).expect("decompress");
    std::fs::write(&path, data).expect("write the fixture");
    path
}

#[test]
fn reads_a_real_ntfs_windows_installation() {
    let path = fixture();
    let device = FileDevice::open(&path).expect("open the image");
    let kind = detect_filesystem(&device);
    assert_eq!(kind, FileSystemKind::Ntfs);

    let size = device.size().unwrap();
    let partition = PartitionDevice::new(Arc::new(device), 0, size);
    let mut volume = open_filesystem(partition, kind).expect("open the NTFS volume");

    // The Windows boot manager, the kernel and the BCD store have to be
    // found.
    let probe = windows::probe(volume.as_mut());
    assert!(probe.looks_like_windows(), "probe: {probe:?}");
    assert!(probe.winload, "winload.efi was not found");
    assert!(probe.ntoskrnl, "ntoskrnl.exe was not found");
    assert!(probe.bcd, "the BCD store was not found");
    assert!(probe.is_bootable());

    // The directory listing has to work.
    let system32 = volume.list_dir("/Windows/System32").expect("list System32");
    let names: Vec<String> = system32.into_iter().map(|entry| entry.name).collect();
    assert!(names
        .iter()
        .any(|name| name.eq_ignore_ascii_case("winload.efi")));
    assert!(names
        .iter()
        .any(|name| name.eq_ignore_ascii_case("ntoskrnl.exe")));

    // The file content has to be readable.
    let content = volume
        .read_file("/Windows/System32/winload.efi", 4096)
        .expect("read winload.efi");
    assert_eq!(content, b"fake-winload\n");
    assert_eq!(volume.file_size("/Windows/System32/ntoskrnl.exe"), Some(12));

    let _ = std::fs::remove_file(&path);
}
