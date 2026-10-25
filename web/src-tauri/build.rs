//! Tauri's code generation, which has to run before the crate compiles.
//!
//! It reads `tauri.conf.json` and the `capabilities/` directory and writes the
//! permission schemas and the context `tauri::generate_context!` expands into. The
//! crate does not compile without it — declaring `tauri-build` in `[build-dependencies]`
//! and forgetting this file is the first way a Tauri crate fails, and it fails with
//! a macro error that names none of this.
//!
//! The app manifest lists our own commands so that each one gets an
//! `allow-<command>` permission. Without it, every command in
//! `generate_handler!` is callable from any window with no ACL check at all, and
//! `capabilities/default.json` gates only the plugins.
// Returned, not unwrapped: cargo fails the build on an `Err` from a build script's `main`.
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "loopback_authorize",
            "open_external",
            "credential_save",
            "credential_load",
            "credential_clear",
            "credential_session",
        ]),
    ))?;
    Ok(())
}
