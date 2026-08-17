//! Tauri's code generation, which has to run before the crate compiles.
//!
//! It reads `tauri.conf.json` and the `capabilities/` directory and writes the
//! permission schemas and the context `tauri::generate_context!` expands into. The
//! crate does not compile without it — declaring `tauri-build` in `[build-dependencies]`
//! and forgetting this file is the first way a Tauri crate fails, and it fails with
//! a macro error that names none of this.
fn main() {
    tauri_build::build();
}
