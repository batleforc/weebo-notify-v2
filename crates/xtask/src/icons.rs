//! Rasterising the desktop icons from their SVG sources (design §10bis.3ter, M5).
//!
//! # Why the sources are SVG and the outputs are committed
//!
//! Tauri hands the OS a PNG: a tray icon and a bundle icon are files, not markup.
//! But a committed PNG is a blob nobody can review — a change to the mark would
//! arrive as "binary files differ", which is exactly the diff a design decision must
//! not hide. So the mark is authored once in SVG, next to a comment explaining every
//! choice in it, and this turns it into the rasters the shell loads.
//!
//! # Why pure Rust
//!
//! `librsvg`, Inkscape and `ImageMagick` are system packages. This workspace has no
//! root (§12.4) — the same constraint that stops the shell itself being built here —
//! so a chore that needed one of them would be a chore nobody in a Che workspace
//! could run. `resvg` is Rust and has no system dependency, which makes this the one
//! part of M5 that works everywhere today.

use std::path::Path;

use anyhow::Context as _;

/// One mark, and the sizes the shell wants it at.
struct Mark {
    /// File stem, shared by the source and every output.
    name: &'static str,
    /// Square sizes to emit, in pixels.
    sizes: &'static [u32],
}

/// What gets generated.
///
/// Tray marks are small by nature — a menu bar is 16 logical pixels, so 32 and 64
/// cover the usual 2× and 4× scales and anything larger is bytes nobody samples.
/// The app icon is one large square: `tauri icon` derives the platform set (`.ico`,
/// `.icns`, the Android and iOS densities) from a single big PNG, and duplicating
/// that here would be re-implementing a tool the CLI already ships.
const MARKS: &[Mark] = &[
    Mark {
        name: "tray-neutral",
        sizes: &[32, 64],
    },
    Mark {
        name: "tray-unread",
        sizes: &[32, 64],
    },
    Mark {
        name: "tray-dnd",
        sizes: &[32, 64],
    },
    Mark {
        name: "tray-offline",
        sizes: &[32, 64],
    },
    Mark {
        name: "icon",
        sizes: &[512, 1024],
    },
];

/// Render every mark under `icons/` from its source in `icons/src/`.
pub(crate) fn generate(root: &Path) -> anyhow::Result<()> {
    let sources = root.join("src");

    for mark in MARKS {
        let source = sources.join(format!("{}.svg", mark.name));
        let svg =
            std::fs::read(&source).with_context(|| format!("reading {}", source.display()))?;

        // Parsed once per mark rather than once per size: the tree is the expensive
        // half, and the sizes differ only by the transform below.
        let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())
            .with_context(|| format!("parsing {}", source.display()))?;

        for &size in mark.sizes {
            let written = render(&tree, size, &output(root, mark.name, size))?;
            println!("{}", written.display());
        }
    }

    Ok(())
}

/// `tray-neutral.png` for the first size, `tray-neutral@64.png` for the rest.
///
/// The unsuffixed name is what `main.rs` asks for, so the smallest size is the one
/// that keeps it: a shell that hard-coded `@32` would have to change every time a
/// scale was added.
fn output(root: &Path, name: &str, size: u32) -> std::path::PathBuf {
    let mark = MARKS.iter().find(|mark| mark.name == name);
    let first = mark.and_then(|mark| mark.sizes.first().copied());

    if first == Some(size) {
        root.join(format!("{name}.png"))
    } else {
        root.join(format!("{name}@{size}.png"))
    }
}

/// Draw one tree at one square size.
fn render(tree: &resvg::usvg::Tree, size: u32, to: &Path) -> anyhow::Result<std::path::PathBuf> {
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(size, size).context("allocating the target bitmap")?;

    // Uniform scale from the source's own viewBox, so a mark authored on a 512
    // canvas and one authored on a 24 canvas both fill the output. Reading the
    // tree's size rather than assuming 512 is what stops the next mark being
    // silently cropped.
    #[allow(
        clippy::cast_precision_loss,
        reason = "icon sizes are small integers; the nearest f32 is exact here"
    )]
    let scale = size as f32 / tree.size().width().max(tree.size().height());

    resvg::render(
        tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    pixmap
        .save_png(to)
        .with_context(|| format!("writing {}", to.display()))?;

    Ok(to.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smallest_size_keeps_the_unsuffixed_name() {
        // `main.rs` asks for `tray-neutral.png`. If a scale were added below 32 and
        // this rule silently moved the plain name onto it, the shell would keep
        // loading a file that had changed meaning.
        let root = Path::new("/icons");
        assert_eq!(
            output(root, "tray-neutral", 32),
            Path::new("/icons/tray-neutral.png")
        );
        assert_eq!(
            output(root, "tray-neutral", 64),
            Path::new("/icons/tray-neutral@64.png")
        );
    }

    #[test]
    fn every_mark_the_shell_names_is_generated() {
        // The list in `main.rs` and the list here are two lists, and this is the
        // only thing holding them together. A tray variant added to §10bis.3ter's
        // table and forgotten here would fail at runtime on one state, which is the
        // hardest kind of missing file to notice.
        for name in [
            "tray-neutral",
            "tray-unread",
            "tray-dnd",
            "tray-offline",
            "icon",
        ] {
            assert!(
                MARKS.iter().any(|mark| mark.name == name),
                "{name} is loaded by the shell and not generated here"
            );
        }
    }
}
