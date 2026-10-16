//! Platform conformance: the same assertions against the real OS and the
//! simulation.  Every build target runs these (PLAN.md, "Build targets").

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use nucleus_platform::real::RealPlatform;
use nucleus_platform::sim::SimPlatform;
use nucleus_platform::{OpenMode, Platform};

/// A fresh directory under the OS temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "nucleus-conformance-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs `check` against the real OS (in a temp dir) and the simulation.
fn both(check: impl Fn(&dyn Platform, &Path)) {
    let tmp = TempDir::new();
    check(&RealPlatform::new().unwrap(), &tmp.0);
    let sim = SimPlatform::new(1);
    sim.fs().create_dir_all(Path::new("/db")).unwrap();
    check(&sim, Path::new("/db"));
}

#[test]
fn write_then_read() {
    both(|p, root| {
        let f = p.fs().open(&root.join("a"), OpenMode::Create).unwrap();
        f.write_all_at(b"hello world", 0).unwrap();
        f.write_all_at(b"W", 6).unwrap();
        let mut buf = [0u8; 11];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello World");
        assert_eq!(f.size().unwrap(), 11);
        f.sync().unwrap();
    });
}

#[test]
fn reads_past_the_end() {
    both(|p, root| {
        let f = p.fs().open(&root.join("a"), OpenMode::Create).unwrap();
        f.write_all_at(b"abc", 0).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(f.read_at(&mut buf, 3).unwrap(), 0);
        assert_eq!(f.read_at(&mut buf, 100).unwrap(), 0);
        assert_eq!(f.read_at(&mut buf, 1).unwrap(), 2);
        assert_eq!(&buf[..2], b"bc");
        let err = f.read_exact_at(&mut buf, 0).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    });
}

#[test]
fn writes_past_the_end_zero_fill() {
    both(|p, root| {
        let f = p.fs().open(&root.join("a"), OpenMode::Create).unwrap();
        f.write_all_at(b"x", 4).unwrap();
        let mut buf = [9u8; 5];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"\0\0\0\0x");
    });
}

#[test]
fn set_len_truncates_and_extends() {
    both(|p, root| {
        let f = p.fs().open(&root.join("a"), OpenMode::Create).unwrap();
        f.write_all_at(b"abcdef", 0).unwrap();
        f.set_len(2).unwrap();
        assert_eq!(f.size().unwrap(), 2);
        f.set_len(4).unwrap();
        let mut buf = [9u8; 4];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"ab\0\0");
    });
}

#[test]
fn open_modes() {
    both(|p, root| {
        let path = root.join("a");
        let missing = |m| p.fs().open(&path, m).err().map(|e| e.kind());
        assert_eq!(missing(OpenMode::ReadOnly), Some(ErrorKind::NotFound));
        assert_eq!(missing(OpenMode::ReadWrite), Some(ErrorKind::NotFound));

        drop(p.fs().open(&path, OpenMode::CreateNew).unwrap());
        let again = p
            .fs()
            .open(&path, OpenMode::CreateNew)
            .err()
            .map(|e| e.kind());
        assert_eq!(again, Some(ErrorKind::AlreadyExists));

        // Create opens an existing file without truncating it.
        p.fs()
            .open(&path, OpenMode::ReadWrite)
            .unwrap()
            .write_all_at(b"keep", 0)
            .unwrap();
        let f = p.fs().open(&path, OpenMode::Create).unwrap();
        assert_eq!(f.size().unwrap(), 4);

        let ro = p.fs().open(&path, OpenMode::ReadOnly).unwrap();
        assert!(ro.write_at(b"x", 0).is_err());
    });
}

#[test]
fn open_needs_an_existing_parent() {
    both(|p, root| {
        let err = p
            .fs()
            .open(&root.join("no/such/dir/a"), OpenMode::Create)
            .err();
        assert_eq!(err.map(|e| e.kind()), Some(ErrorKind::NotFound));
    });
}

#[test]
fn rename_replaces_atomically() {
    both(|p, root| {
        let (a, b) = (root.join("a"), root.join("b"));
        p.fs()
            .open(&a, OpenMode::Create)
            .unwrap()
            .write_all_at(b"new", 0)
            .unwrap();
        p.fs()
            .open(&b, OpenMode::Create)
            .unwrap()
            .write_all_at(b"old!", 0)
            .unwrap();
        p.fs().rename(&a, &b).unwrap();
        assert!(!p.fs().exists(&a).unwrap());
        let f = p.fs().open(&b, OpenMode::ReadOnly).unwrap();
        assert_eq!(f.size().unwrap(), 3);
        p.fs().sync_dir(root).unwrap();
    });
}

#[test]
fn directories() {
    both(|p, root| {
        p.fs().create_dir_all(&root.join("x/y")).unwrap();
        p.fs().create_dir_all(&root.join("x/y")).unwrap(); // idempotent
        for name in ["b", "a", "c"] {
            drop(
                p.fs()
                    .open(&root.join("x").join(name), OpenMode::Create)
                    .unwrap(),
            );
        }
        let names = p.fs().list_dir(&root.join("x")).unwrap();
        let want: Vec<OsString> = ["a", "b", "c", "y"].iter().map(OsString::from).collect();
        assert_eq!(names, want);

        p.fs().remove_file(&root.join("x/b")).unwrap();
        assert!(!p.fs().exists(&root.join("x/b")).unwrap());
        let err = p.fs().remove_file(&root.join("x/b")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        p.fs().sync_dir(&root.join("x")).unwrap();
        assert!(p.fs().open(&root.join("x"), OpenMode::ReadOnly).is_err());
    });
}

#[test]
fn clock_is_monotonic() {
    both(|p, _| {
        let a = p.clock().now();
        let b = p.clock().now();
        assert!(b >= a);
        assert!(p.clock().wall() > std::time::UNIX_EPOCH);
    });
}

#[test]
fn rng_varies() {
    both(|p, _| {
        let draws: Vec<u64> = (0..16).map(|_| p.rng().next_u64()).collect();
        let mut distinct = draws.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), draws.len());
        let mut buf = [0u8; 13];
        p.rng().fill_bytes(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    });
}
