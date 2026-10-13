//! Regression test for the exe icon (`build.rs` + `icon/`).
//!
//! `build.rs` hands `icon/app.rc` to windres and passes the resulting object to the
//! linker, so the shipped `icon/repo-manager.ico` has to stay a valid multi-size ICO:
//! a truncated file still builds, but Windows then silently falls back to the default
//! application icon. `icon/make_ico.ps1` regenerates it from `rabbit.png`.

use std::path::PathBuf;

fn repo_file(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn u16_le(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The resource script must keep referencing the icon that is committed next to it,
/// otherwise windres produces an object without any image and nothing warns about it.
#[test]
fn resource_script_references_the_committed_icon() {
    let text = std::fs::read_to_string(repo_file("icon/app.rc")).expect("icon/app.rc");
    assert!(
        text.contains("1 ICON \"repo-manager.ico\""),
        "icon/app.rc must embed icon/repo-manager.ico, got: {text}"
    );
    assert!(
        std::fs::metadata(repo_file("icon/repo-manager.ico")).is_ok(),
        "icon/repo-manager.ico is missing, run icon/make_ico.ps1"
    );
}

#[test]
fn icon_directory_points_inside_the_file() {
    let bytes = std::fs::read(repo_file("icon/repo-manager.ico")).expect("icon/repo-manager.ico");
    assert!(bytes.len() > 6 + 16, "ico too short: {}", bytes.len());
    assert_eq!(u16_le(&bytes, 0), 0, "reserved must be 0");
    assert_eq!(u16_le(&bytes, 2), 1, "resource type must be 1 (icon)");
    let count = u16_le(&bytes, 4) as usize;
    assert!(count >= 4, "expected several sizes, got {count}");
    let mut edges: Vec<u32> = Vec::new();
    for index in 0..count {
        let entry = 6 + index * 16;
        // 256 px is stored as 0, which is exactly what Explorer needs on a desktop
        let edge = if bytes[entry] == 0 { 256 } else { bytes[entry] as u32 };
        let size = u32_le(&bytes, entry + 8) as usize;
        let offset = u32_le(&bytes, entry + 12) as usize;
        assert!(size > 0 && offset + size <= bytes.len(), "entry {index} ({edge}px) runs past the end of the file: offset {offset} + size {size} > {}", bytes.len());
        let header = offset;
        assert_eq!(u32_le(&bytes, header), 40, "entry {index} must carry a 40-byte BITMAPINFOHEADER");
        let width = u32_le(&bytes, header + 4) as i64;
        let height = u32_le(&bytes, header + 8) as i64;
        assert_eq!(width, edge as i64, "entry {index} claims {width}px wide but the directory says {edge}px");
        // biHeight counts the colour bitmap *and* the AND mask, so it is twice the width
        assert_eq!(height, width * 2, "entry {index}: biHeight {height} must be 2 x biWidth {width}");
        assert_eq!(u16_le(&bytes, header + 14), 32, "entry {index} must be 32bpp");
        edges.push(edge);
    }
    assert!(edges.contains(&256), "no 256px image, desktop shortcuts get blurry: {edges:?}");
    let mut sorted = edges.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), edges.len(), "every image must have its own size, got {edges:?}");
}