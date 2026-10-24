// `sqlx::migrate!` embeds `migrations/` at compile time, and cargo only rebuilds on a
// changed source file: without this, a new migration ships in no binary until something
// else in the crate changes (and the database then refuses it as `VersionMissing`).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
