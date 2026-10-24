//! Rasterises the desktop icons from their SVG sources (§10bis.3ter), in pure Rust so it
//! runs without system packages. Sources stay SVG so mark changes are reviewable.

use std::path::Path;

use anyhow::Context as _;

/// One mark, and the sizes the shell wants it at.
struct Mark {
    /// File stem, shared by the source and every output.
    name: &'static str,
    /// Square sizes to emit, in pixels.
    sizes: &'static [u32],
}

/// What gets generated: small tray sizes, and one large app icon `tauri icon` derives the rest from.
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

        let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())
            .with_context(|| format!("parsing {}", source.display()))?;

        for &size in mark.sizes {
            let written = render(&tree, size, &output(root, mark.name, size))?;
            println!("{}", written.display());
        }
    }

    Ok(())
}

/// `tray-neutral.png` for the first size (the name `main.rs` loads), `tray-neutral@64.png` for the rest.
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

    // Scale from the source's own viewBox so marks on any canvas fill the output.
    #[expect(
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
        // `main.rs` loads the unsuffixed name; it must stay on the smallest size.
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
        // Must match the tray variants `main.rs` loads.
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
