// The graphical interface of Helm.
//
// The binary is a thin wrapper around the library: it only starts the
// interface, everything else happens in `helm_gui_lib`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    helm_gui_lib::run();
}
