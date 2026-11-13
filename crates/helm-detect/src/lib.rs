//! Detection of the operating systems installed on the machine.
//!
//! The detection is done by reading the disks directly: the partition tables
//! are parsed from the raw devices and the filesystems are inspected without
//! mounting them. That way Helm sees every disk of the machine, including the
//! ones that hold an operating system it is not currently running under.

pub mod block;
pub mod detect;
pub mod disk;
pub mod esp;
pub mod fs;
pub mod gpt;
pub mod os;

pub use detect::{detect, detect_disks};
pub use disk::RawDisk;
pub use gpt::Gpt;
