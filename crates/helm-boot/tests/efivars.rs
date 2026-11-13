//! Tests that run against the firmware variables of the machine.
//!
//! The tests are skipped when the machine did not boot through UEFI or when
//! the firmware variables are not readable.

use helm_boot::efivars::EfiVarStore;
use helm_boot::store::{decode_boot_entries, FirmwareStore};

fn store() -> Option<EfiVarStore> {
    let store = EfiVarStore::open().ok()?;
    // The variables have to be readable, otherwise the test is useless.
    store.read("BootOrder").ok()?;
    Some(store)
}

#[test]
fn lists_the_variables_of_the_machine() {
    let Some(store) = store() else {
        eprintln!("skipping: the firmware variables are not readable");
        return;
    };
    let names = store.list().unwrap();
    assert!(
        names.iter().any(|name| name == "BootOrder"),
        "BootOrder is missing from {names:?}"
    );
    // The names must not contain the GUID any more.
    assert!(names
        .iter()
        .all(|name| !name.contains('-') || name.len() <= 16));
}

#[test]
fn decodes_the_boot_entries_of_the_machine() {
    let Some(store) = store() else {
        eprintln!("skipping: the firmware variables are not readable");
        return;
    };
    let entries = decode_boot_entries(&store).unwrap();
    for entry in &entries {
        // Every entry must have a description and a device path.
        assert!(!entry.description.is_empty());
        assert!(entry.device_path_hex.is_some());
    }
    println!("the firmware has {} boot entries", entries.len());
    for entry in &entries {
        println!(
            "  {} -> {} ({:?})",
            entry.var_name, entry.description, entry.loader_path
        );
    }
}
