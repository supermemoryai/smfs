//! Conformance tests for SupermemoryFs — mirrors the MemFs tests in vfs/mem.rs.
//!
//! Each test creates a fresh in-memory SQLite database, constructs a
//! SupermemoryFs, and exercises the same operations as the MemFs conformance
//! suite. If these tests pass, SupermemoryFs is a drop-in replacement.

use std::sync::Arc;

use super::db::Db;
use super::fs::SupermemoryFs;
use crate::vfs::mode::{S_IFDIR, S_IFMT, S_IFREG};
use crate::vfs::traits::FileSystem;
use crate::vfs::types::{SetAttr, TimeOrNow, Timestamp};
use crate::vfs::VfsError;

const UID: u32 = 1000;
const GID: u32 = 1000;
const ROOT: u64 = 1;

fn fs() -> SupermemoryFs {
    let db = Arc::new(Db::open_in_memory().unwrap());
    SupermemoryFs::new(db)
}

// ─── Root and sanity ────────────────────────────────────────────────

#[tokio::test]
async fn test_root_exists() {
    let fs = fs();
    let attr = fs.getattr(ROOT).await.unwrap().expect("root must exist");
    assert!(attr.is_directory());
    assert_eq!(attr.ino, ROOT);
}

#[tokio::test]
async fn test_root_readdir_empty() {
    let fs = fs();
    let names = fs.readdir(ROOT).await.unwrap().unwrap();
    assert!(names.is_empty());
}

#[tokio::test]
async fn test_getattr_nonexistent_returns_none() {
    let fs = fs();
    assert!(fs.getattr(999).await.unwrap().is_none());
}

#[tokio::test]
async fn test_lookup_in_empty_root_returns_none() {
    let fs = fs();
    assert!(fs.lookup(ROOT, "nope").await.unwrap().is_none());
}

// ─── Directory creation and removal ─────────────────────────────────

#[tokio::test]
async fn test_mkdir_creates_entry() {
    let fs = fs();
    let dir = fs.mkdir(ROOT, "foo", 0o755, UID, GID).await.unwrap();
    assert!(dir.is_directory());

    let names = fs.readdir(ROOT).await.unwrap().unwrap();
    assert_eq!(names, vec!["foo".to_string()]);
}

#[tokio::test]
async fn test_mkdir_returns_correct_attr() {
    let fs = fs();
    let dir = fs.mkdir(ROOT, "foo", 0o755, UID, GID).await.unwrap();
    assert_eq!(dir.mode & S_IFMT, S_IFDIR);
    assert_eq!(dir.mode & 0o777, 0o755);
    assert_eq!(dir.uid, UID);
    assert_eq!(dir.gid, GID);
    assert_eq!(dir.nlink, 2);
}

#[tokio::test]
async fn test_mkdir_same_name_twice_fails() {
    let fs = fs();
    fs.mkdir(ROOT, "foo", 0o755, UID, GID).await.unwrap();
    let err = fs.mkdir(ROOT, "foo", 0o755, UID, GID).await.unwrap_err();
    assert!(matches!(err, VfsError::AlreadyExists));
}

#[tokio::test]
async fn test_rmdir_empty_works() {
    let fs = fs();
    fs.mkdir(ROOT, "tmp", 0o755, UID, GID).await.unwrap();
    fs.rmdir(ROOT, "tmp").await.unwrap();
    assert!(fs.lookup(ROOT, "tmp").await.unwrap().is_none());
}

#[tokio::test]
async fn test_rmdir_nonempty_returns_not_empty() {
    let fs = fs();
    let dir = fs.mkdir(ROOT, "d", 0o755, UID, GID).await.unwrap();
    fs.create_file(dir.ino, "inside", 0o644, UID, GID)
        .await
        .unwrap();
    let err = fs.rmdir(ROOT, "d").await.unwrap_err();
    assert!(matches!(err, VfsError::NotEmpty));
}

#[tokio::test]
async fn test_rmdir_nonexistent_returns_not_found() {
    let fs = fs();
    let err = fs.rmdir(ROOT, "nope").await.unwrap_err();
    assert!(matches!(err, VfsError::NotFound));
}

// ─── Regular files ──────────────────────────────────────────────────

#[tokio::test]
async fn test_create_file_returns_handle_and_attr() {
    let fs = fs();
    let (attr, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    assert!(attr.is_file());
    assert_eq!(attr.mode & 0o777, 0o644);
    assert_eq!(attr.size, 0);
    let empty = handle.read(0, 100).await.unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn test_write_then_read_round_trip() {
    let fs = fs();
    let (_, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    let n = handle.write(0, b"hello world").await.unwrap();
    assert_eq!(n, 11);
    let data = handle.read(0, 100).await.unwrap();
    assert_eq!(data, b"hello world");
}

#[tokio::test]
async fn test_write_at_offset_extends_file() {
    let fs = fs();
    let (_, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    handle.write(10, b"hi").await.unwrap();
    let attr = handle.getattr().await.unwrap();
    assert_eq!(attr.size, 12);
    let data = handle.read(0, 100).await.unwrap();
    assert_eq!(&data[10..12], b"hi");
    assert_eq!(&data[0..10], &[0; 10]);
}

#[tokio::test]
async fn test_read_past_eof_returns_empty() {
    let fs = fs();
    let (_, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    handle.write(0, b"abc").await.unwrap();
    let data = handle.read(100, 10).await.unwrap();
    assert!(data.is_empty());
}

#[tokio::test]
async fn test_read_empty_file_returns_empty() {
    let fs = fs();
    let (_, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    let data = handle.read(0, 100).await.unwrap();
    assert!(data.is_empty());
}

#[tokio::test]
async fn test_create_file_same_name_twice_fails() {
    let fs = fs();
    fs.create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    let err = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap_err();
    assert!(matches!(err, VfsError::AlreadyExists));
}

#[tokio::test]
async fn test_unlink_removes_entry() {
    let fs = fs();
    fs.create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    fs.unlink(ROOT, "a.txt").await.unwrap();
    assert!(fs.lookup(ROOT, "a.txt").await.unwrap().is_none());
}

#[tokio::test]
async fn test_unlink_nonexistent_returns_not_found() {
    let fs = fs();
    let err = fs.unlink(ROOT, "nope").await.unwrap_err();
    assert!(matches!(err, VfsError::NotFound));
}

#[tokio::test]
async fn test_unlink_directory_returns_is_a_directory() {
    let fs = fs();
    fs.mkdir(ROOT, "d", 0o755, UID, GID).await.unwrap();
    let err = fs.unlink(ROOT, "d").await.unwrap_err();
    assert!(matches!(err, VfsError::IsADirectory));
}

// ─── Readdir variants ───────────────────────────────────────────────

#[tokio::test]
async fn test_readdir_lists_all_children_sorted() {
    let fs = fs();
    fs.create_file(ROOT, "b.txt", 0o644, UID, GID)
        .await
        .unwrap();
    fs.create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    fs.mkdir(ROOT, "c", 0o755, UID, GID).await.unwrap();

    let names = fs.readdir(ROOT).await.unwrap().unwrap();
    assert_eq!(names, vec!["a.txt", "b.txt", "c"]);
}

#[tokio::test]
async fn test_readdir_on_file_returns_none() {
    let fs = fs();
    let (attr, _) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    assert!(fs.readdir(attr.ino).await.unwrap().is_none());
}

#[tokio::test]
async fn test_readdir_plus_includes_attrs() {
    let fs = fs();
    let (file_attr, _) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    let entries = fs.readdir_plus(ROOT).await.unwrap().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "a.txt");
    assert_eq!(entries[0].attr.ino, file_attr.ino);
}

// ─── Rename ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_rename_within_same_directory() {
    let fs = fs();
    let (_, handle) = fs
        .create_file(ROOT, "old.txt", 0o644, UID, GID)
        .await
        .unwrap();
    handle.write(0, b"hi").await.unwrap();

    fs.rename(ROOT, "old.txt", ROOT, "new.txt").await.unwrap();
    assert!(fs.lookup(ROOT, "old.txt").await.unwrap().is_none());
    let moved = fs.lookup(ROOT, "new.txt").await.unwrap().unwrap();
    assert_eq!(moved.size, 2);
}

#[tokio::test]
async fn test_rename_across_directories() {
    let fs = fs();
    let src_dir = fs.mkdir(ROOT, "src", 0o755, UID, GID).await.unwrap();
    let dst_dir = fs.mkdir(ROOT, "dst", 0o755, UID, GID).await.unwrap();
    fs.create_file(src_dir.ino, "f", 0o644, UID, GID)
        .await
        .unwrap();

    fs.rename(src_dir.ino, "f", dst_dir.ino, "f").await.unwrap();
    assert!(fs.lookup(src_dir.ino, "f").await.unwrap().is_none());
    assert!(fs.lookup(dst_dir.ino, "f").await.unwrap().is_some());
}

#[tokio::test]
async fn test_rename_nonexistent_returns_not_found() {
    let fs = fs();
    let err = fs.rename(ROOT, "nope", ROOT, "whatever").await.unwrap_err();
    assert!(matches!(err, VfsError::NotFound));
}

#[tokio::test]
async fn test_rename_over_existing_file_replaces() {
    let fs = fs();
    let (_, src_handle) = fs.create_file(ROOT, "src", 0o644, UID, GID).await.unwrap();
    src_handle.write(0, b"new").await.unwrap();
    fs.create_file(ROOT, "dst", 0o644, UID, GID).await.unwrap();

    fs.rename(ROOT, "src", ROOT, "dst").await.unwrap();
    assert!(fs.lookup(ROOT, "src").await.unwrap().is_none());
    let dst = fs.lookup(ROOT, "dst").await.unwrap().unwrap();
    assert_eq!(dst.size, 3);
}

// ─── Setattr ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_setattr_truncate_via_size() {
    let fs = fs();
    let (attr, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    handle.write(0, b"hello world").await.unwrap();
    let updated = fs
        .setattr(
            attr.ino,
            SetAttr {
                size: Some(5),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.size, 5);
    let data = handle.read(0, 100).await.unwrap();
    assert_eq!(data, b"hello");
}

#[tokio::test]
async fn test_setattr_chmod_via_mode() {
    let fs = fs();
    let (attr, _) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    let updated = fs
        .setattr(
            attr.ino,
            SetAttr {
                mode: Some(0o600),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.mode & 0o777, 0o600);
    assert_eq!(updated.mode & S_IFMT, S_IFREG);
}

#[tokio::test]
async fn test_setattr_chown_via_uid_gid() {
    let fs = fs();
    let (attr, _) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    let updated = fs
        .setattr(
            attr.ino,
            SetAttr {
                uid: Some(42),
                gid: Some(99),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.uid, 42);
    assert_eq!(updated.gid, 99);
}

#[tokio::test]
async fn test_setattr_utimens_via_mtime() {
    let fs = fs();
    let (attr, _) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    let target = Timestamp {
        sec: 1_700_000_000,
        nsec: 500,
    };
    let updated = fs
        .setattr(
            attr.ino,
            SetAttr {
                mtime: Some(TimeOrNow::Time(target)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.mtime, target);
}

// ─── Symlinks ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_symlink_create_and_readlink() {
    let fs = fs();
    let attr = fs
        .symlink(ROOT, "link", "/some/target", UID, GID)
        .await
        .unwrap();
    assert!(attr.is_symlink());
    assert_eq!(attr.size, "/some/target".len() as u64);
    let target = fs.readlink(attr.ino).await.unwrap().unwrap();
    assert_eq!(target, "/some/target");
}

#[tokio::test]
async fn test_readlink_on_regular_file_returns_error() {
    let fs = fs();
    let (attr, _) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    let err = fs.readlink(attr.ino).await.unwrap_err();
    assert!(matches!(err, VfsError::NotASymlink));
}

// ─── Hard links ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_link_creates_second_name() {
    let fs = fs();
    let (attr, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    handle.write(0, b"data").await.unwrap();

    let linked = fs.link(attr.ino, ROOT, "b.txt").await.unwrap();
    assert_eq!(linked.nlink, 2);

    let via_a = fs.lookup(ROOT, "a.txt").await.unwrap().unwrap();
    let via_b = fs.lookup(ROOT, "b.txt").await.unwrap().unwrap();
    assert_eq!(via_a.ino, via_b.ino);
}

#[tokio::test]
async fn test_unlink_one_name_keeps_other() {
    let fs = fs();
    let (attr, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    handle.write(0, b"shared").await.unwrap();
    fs.link(attr.ino, ROOT, "b.txt").await.unwrap();

    fs.unlink(ROOT, "a.txt").await.unwrap();
    assert!(fs.lookup(ROOT, "a.txt").await.unwrap().is_none());

    let remaining = fs.lookup(ROOT, "b.txt").await.unwrap().unwrap();
    assert_eq!(remaining.size, 6);
    assert_eq!(remaining.nlink, 1);
}

// ─── statfs ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_statfs_counts_inodes_and_bytes() {
    let fs = fs();
    let (_, handle) = fs
        .create_file(ROOT, "a.txt", 0o644, UID, GID)
        .await
        .unwrap();
    handle.write(0, b"12345").await.unwrap();

    let stats = fs.statfs().await.unwrap();
    assert!(stats.inodes >= 2);
    assert_eq!(stats.bytes_used, 5);
}

// ─── Binary upload + sibling transcripts + poison-job ───────────────

use crate::api::Document;
use crate::cache::db::PushOp;

fn doc_with(id: &str, filepath: &str, type_: &str, content: &str, status: &str) -> Document {
    Document {
        id: id.to_string(),
        filepath: Some(filepath.to_string()),
        custom_id: None,
        title: None,
        summary: None,
        content: Some(content.to_string()),
        status: status.to_string(),
        container_tags: None,
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: "2026-01-01T00:00:00.000Z".to_string(),
        metadata: None,
        type_: Some(type_.to_string()),
        url: None,
    }
}

#[tokio::test]
async fn test_reconcile_image_synthesizes_transcription_sibling() {
    let fs = fs();
    let doc = doc_with("d1", "/cat.png", "image", "a cat on a couch", "done");
    fs.reconcile_one(&doc).unwrap();
    let attr = fs
        .lookup(ROOT, "cat.png.image-transcription.md")
        .await
        .unwrap()
        .expect("transcription sibling must exist");
    assert_eq!(attr.mode & 0o777, 0o444);
    assert!(fs.db().is_derived(attr.ino));
    assert_eq!(attr.size as usize, "a cat on a couch".len());
}

#[tokio::test]
async fn test_reconcile_pdf_synthesizes_pdf_transcription_sibling() {
    let fs = fs();
    let doc = doc_with("d2", "/notes.pdf", "pdf", "extracted page text", "done");
    fs.reconcile_one(&doc).unwrap();
    assert!(fs
        .lookup(ROOT, "notes.pdf.pdf-transcription.md")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_reconcile_text_creates_no_sibling() {
    let fs = fs();
    let doc = doc_with("d3", "/plain.md", "text", "hello", "done");
    fs.reconcile_one(&doc).unwrap();
    // No sibling suffixed file should exist.
    for suffix in &[
        ".image-transcription.md",
        ".pdf-transcription.md",
        ".video-transcription.md",
        ".audio-transcription.md",
        ".webpage-transcription.md",
    ] {
        let name = format!("plain.md{}", suffix);
        assert!(
            fs.lookup(ROOT, &name).await.unwrap().is_none(),
            "unexpected sibling {name}"
        );
    }
}

#[tokio::test]
async fn test_derived_inode_never_enters_push_queue() {
    let fs = fs();
    let doc = doc_with("d4", "/cat.png", "image", "description", "done");
    fs.reconcile_one(&doc).unwrap();
    let sibling = fs
        .lookup(ROOT, "cat.png.image-transcription.md")
        .await
        .unwrap()
        .unwrap();
    // Even if we were to mark it dirty, claim_next should not see it because
    // we never enqueue derived inodes; verify by checking push_queue is empty.
    fs.db().set_dirty_since(sibling.ino, Some(1));
    let now = 10_000_000_000;
    assert!(
        fs.db().push_queue_claim_next(now).is_none(),
        "derived inode must not be claimed by push worker"
    );
}

#[tokio::test]
async fn test_poison_skips_claim_next() {
    let fs = fs();
    let (attr, _) = fs
        .create_file(ROOT, "bad.xyz", 0o644, UID, GID)
        .await
        .unwrap();
    fs.db()
        .push_queue_upsert("/bad.xyz", PushOp::Create, Some(attr.ino), None, None, 1);
    fs.db().push_queue_poison("/bad.xyz", 415, "unsupported", 2);
    assert!(
        fs.db().push_queue_claim_next(10).is_none(),
        "poisoned row must not be claimed"
    );
}

#[tokio::test]
async fn test_create_derived_sibling_writes_readonly_file() {
    let fs = fs();
    let _ino = fs
        .create_derived_sibling("/orphan.smfs-error.txt", "server said: unsupported")
        .unwrap();
    let attr = fs
        .lookup(ROOT, "orphan.smfs-error.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attr.mode & 0o777, 0o444);
    assert!(fs.db().is_derived(attr.ino));
    assert_eq!(attr.size as usize, "server said: unsupported".len());
}

#[tokio::test]
async fn test_unlink_cascade_removes_transcription_sibling() {
    let fs = fs();
    // User writes a raw file locally.
    fs.create_file(ROOT, "cat.png", 0o644, UID, GID)
        .await
        .unwrap();
    // Server-derived transcript sibling is materialized.
    fs.create_derived_sibling("/cat.png.image-transcription.md", "cat photo")
        .unwrap();
    assert!(fs
        .lookup(ROOT, "cat.png.image-transcription.md")
        .await
        .unwrap()
        .is_some());
    fs.unlink(ROOT, "cat.png").await.unwrap();
    assert!(fs.lookup(ROOT, "cat.png").await.unwrap().is_none());
    assert!(
        fs.lookup(ROOT, "cat.png.image-transcription.md")
            .await
            .unwrap()
            .is_none(),
        "sibling transcript must be cascade-removed"
    );
}

#[tokio::test]
async fn test_rename_cascade_moves_transcription_sibling() {
    let fs = fs();
    fs.create_file(ROOT, "cat.png", 0o644, UID, GID)
        .await
        .unwrap();
    fs.create_derived_sibling("/cat.png.image-transcription.md", "content")
        .unwrap();
    fs.rename(ROOT, "cat.png", ROOT, "kitty.png").await.unwrap();
    assert!(fs
        .lookup(ROOT, "cat.png.image-transcription.md")
        .await
        .unwrap()
        .is_none());
    assert!(fs
        .lookup(ROOT, "kitty.png.image-transcription.md")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_binary_content_detection_nonutf8() {
    // JPEG magic bytes: FF D8 FF — definitely not valid UTF-8. Build at
    // runtime so clippy doesn't try to const-evaluate the slice.
    let bytes: Vec<u8> = vec![0xFF, 0xD8, 0xFF];
    assert!(std::str::from_utf8(&bytes).is_err());
    // Plain ASCII is valid UTF-8.
    assert!(std::str::from_utf8(b"hello world").is_ok());
}

#[tokio::test]
async fn test_file_size_cap_enforced() {
    let fs = fs();
    let (_, handle) = fs
        .create_file(ROOT, "big.bin", 0o644, UID, GID)
        .await
        .unwrap();
    // Writing at offset > 100MB must fail.
    let err = handle
        .write(101 * 1024 * 1024, b"x")
        .await
        .expect_err("write past cap must fail");
    assert!(matches!(err, VfsError::InvalidPath(_)));
}
