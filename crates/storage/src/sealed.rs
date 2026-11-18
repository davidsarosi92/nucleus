//! The engines' files sealed under the page key (PLAN.md §M14.28 L4e).
//!
//! On an encrypted database the engines' files — an LSM tree's runs and
//! manifests, the map engine's files, the routes — go through
//! [`SealedFs`]: a [`FileSystem`] that seals what is written to a file
//! and opens what is read, so no engine knows (as nothing above the pager
//! knows a page is sealed).  An engine's file is written once, in order,
//! and read anywhere: the layer seals it in chunks ([`CHUNK`] bytes),
//! each on its own, so a read of a block opens the one or two chunks it
//! falls in — the STREAM construction of online authenticated encryption
//! (Hoang, Reyhanitabar, Rogaway, Vizár, CRYPTO 2015; Tink's streaming
//! AEAD): a chunk's place is its nonce, and whether it is the last is
//! in its associated data, so a file cut short at a chunk's end does not
//! open (its new last chunk was sealed as not last), nor does a chunk
//! moved within it.
//!
//! On disk a sealed file is
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! | 0  | 8  | magic `ncseal\0\x01` |
//! | 8  | 4  | the key's generation (K3f), little-endian |
//! | 12 | 4  | the cipher's code |
//! | 16 | 8  | the file's nonce: random, the first half of each chunk's |
//! | 24 | 4  | the chunk's size |
//! | 28 | 4  | zero |
//! | 32 | …  | the chunks: each its 16-byte tag, then its bytes encrypted |
//!
//! every chunk the chunk's size but the last, which holds 0 to that
//! (an empty file is one empty chunk).  A chunk's nonce is the file's
//! nonce and its index; its associated data the header, its index and
//! whether it is the last.  The ciphers are the pages' — deterministic
//! and misuse resistant —, so a chunk written again under its nonce (the
//! last, as a file grows) gives nothing away but that it is the same.
//!
//! Not caught by the seal: a whole file put in place of another of the
//! same database, as a whole data file put back as it was.
//!
//! A new key (K3f): new files are sealed under it; each file sealed
//! under the key before is sealed again ([`SealedFs::reseal`]) before
//! that key is let go.  A file open meanwhile keeps the key it was opened
//! with: renamed over, it reads what it was opened on.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use nucleus_platform::{File, FileSystem, Lock, OpenMode, Platform};

use crate::PageCipher;

const MAGIC: &[u8; 8] = b"ncseal\0\x01";
/// The header's size.
const HEADER: u64 = 32;
/// A new file's chunk's bytes (but the last's); a file's own is in its
/// header.
pub const CHUNK: usize = 4096;
/// A chunk of [`CHUNK`] bytes on disk: its tag and its bytes.
#[cfg(test)]
const SEALED: u64 = CHUNK as u64 + 16;

fn invalid(path: Option<&Path>, what: &str) -> io::Error {
    let msg = match path {
        Some(p) => format!("{}: {what}", p.display()),
        None => what.to_owned(),
    };
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// The plain length of a sealed file `disk` bytes long (header included)
/// in chunks of `chunk` bytes.
fn plain_len(disk: u64, chunk: u64) -> Option<u64> {
    let sealed = chunk + 16;
    let body = disk.checked_sub(HEADER)?;
    if body < 16 {
        return None;
    }
    let chunks = body.div_ceil(sealed);
    let last = body - (chunks - 1) * sealed - 16;
    if chunks > 1 && last == 0 {
        return None;
    }
    Some((chunks - 1) * chunk + last)
}

/// The engines' file system on an encrypted database: [`FileSystem`]
/// sealing what it writes ([module docs](self)); in the clear, the
/// platform's own.
pub struct SealedFs {
    platform: Arc<dyn Platform>,
    cipher: RwLock<Option<PageCipher>>,
}

impl SealedFs {
    /// Files under `cipher` (`None`: in the clear).
    pub fn new(platform: Arc<dyn Platform>, cipher: Option<PageCipher>) -> Self {
        Self {
            platform,
            cipher: RwLock::new(cipher),
        }
    }

    /// The key new files are sealed under from now on, with the one
    /// before while files are sealed again (K3f).
    pub fn set_cipher(&self, cipher: Option<PageCipher>) {
        *self.cipher.write().unwrap_or_else(|e| e.into_inner()) = cipher;
    }

    fn cipher(&self) -> Option<PageCipher> {
        self.cipher
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether files are sealed.
    pub fn sealed(&self) -> bool {
        self.cipher
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// The generation of the key the file at `path` is sealed under.
    pub fn generation_of(&self, path: &Path) -> io::Result<u32> {
        let f = self.platform.fs().open(path, OpenMode::ReadOnly)?;
        let mut h = [0u8; HEADER as usize];
        f.read_exact_at(&mut h, 0)
            .map_err(|_| invalid(Some(path), "not a sealed file"))?;
        if &h[..8] != MAGIC {
            return Err(invalid(Some(path), "not a sealed file"));
        }
        Ok(u32::from_le_bytes(h[8..12].try_into().expect("4")))
    }

    /// Seals the file at `path` again under the current key, if it is
    /// under another: a copy beside it, synced, renamed over it (its
    /// directory synced by the caller).  Whether it was.
    pub fn reseal(&self, path: &Path) -> io::Result<bool> {
        let Some(c) = self.cipher() else {
            return Ok(false);
        };
        if self.generation_of(path)? == c.generation() {
            return Ok(false);
        }
        let from = self.open(path, OpenMode::ReadOnly)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".reseal");
        let tmp = std::path::PathBuf::from(tmp);
        let fs = self.platform.fs();
        if fs.exists(&tmp)? {
            fs.remove_file(&tmp)?;
        }
        let to = self.open(&tmp, OpenMode::CreateNew)?;
        copy(&*from, &*to)?;
        to.sync()?;
        drop((from, to));
        fs.rename(&tmp, path)?;
        Ok(true)
    }

    /// Seals the file `from`, in the clear, into `to`, synced (a store
    /// being encrypted, [`Store::encrypt`](crate::Store::encrypt)).
    pub fn seal_copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        let fs = self.platform.fs();
        let src = fs.open(from, OpenMode::ReadOnly)?;
        if fs.exists(to)? {
            fs.remove_file(to)?;
        }
        let dst = self.open(to, OpenMode::CreateNew)?;
        copy(&*src, &*dst)?;
        dst.sync()
    }
}

/// `from`'s bytes appended to `to`, a megabyte at a time.
fn copy(from: &dyn File, to: &dyn File) -> io::Result<()> {
    let len = from.size()?;
    let mut buf = vec![0u8; 1 << 20];
    let mut at = 0;
    while at < len {
        let n = buf.len().min((len - at) as usize);
        from.read_exact_at(&mut buf[..n], at)?;
        to.write_all_at(&buf[..n], at)?;
        at += n as u64;
    }
    Ok(())
}

impl FileSystem for SealedFs {
    fn open(&self, path: &Path, mode: OpenMode) -> io::Result<Box<dyn File>> {
        let inner = self.platform.fs().open(path, mode)?;
        match self.cipher() {
            None => Ok(inner),
            Some(c) => Ok(Box::new(SealedFile::open(
                inner,
                &c,
                mode,
                path,
                self.platform.rng().next_u64(),
            )?)),
        }
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.platform.fs().remove_file(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.platform.fs().rename(from, to)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.platform.fs().create_dir_all(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.platform.fs().remove_dir(path)
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<OsString>> {
        self.platform.fs().list_dir(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.platform.fs().exists(path)
    }

    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        self.platform.fs().sync_dir(path)
    }

    fn writes_in_place(&self) -> bool {
        self.platform.fs().writes_in_place()
    }

    fn lock(&self, path: &Path) -> io::Result<Box<dyn Lock>> {
        self.platform.fs().lock(path)
    }
}

/// A file sealed in chunks ([module docs](self)).
struct SealedFile {
    inner: Box<dyn File>,
    /// The key it is sealed under, alone.
    key: PageCipher,
    header: [u8; HEADER as usize],
    /// Its chunks' size (but the last's).
    chunk: u64,
    state: Mutex<State>,
}

struct State {
    /// Its plain length.
    len: u64,
    /// Written to: the last chunk's bytes, kept to seal it again as it
    /// grows.
    tail: Option<Vec<u8>>,
    /// The chunks read last, opened, and the first's index: a scan's
    /// next block often begins in the last of them.
    cached: Option<(u64, Arc<Vec<u8>>)>,
}

impl SealedFile {
    fn open(
        inner: Box<dyn File>,
        cipher: &PageCipher,
        mode: OpenMode,
        path: &Path,
        nonce: u64,
    ) -> io::Result<Self> {
        let disk = inner.size()?;
        if disk == 0 && mode != OpenMode::ReadOnly {
            // A new file: its header, and its one empty chunk.
            let key = cipher.clone().without_previous();
            let mut header = [0u8; HEADER as usize];
            header[..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&key.generation().to_le_bytes());
            header[12..16].copy_from_slice(&key.code().to_le_bytes());
            header[16..24].copy_from_slice(&nonce.to_le_bytes());
            header[24..28].copy_from_slice(&(CHUNK as u32).to_le_bytes());
            inner.write_all_at(&header, 0)?;
            let f = Self {
                inner,
                key,
                header,
                chunk: CHUNK as u64,
                state: Mutex::new(State {
                    len: 0,
                    tail: Some(Vec::new()),
                    cached: None,
                }),
            };
            f.write_chunk(0, &[], true)?;
            return Ok(f);
        }
        let mut header = [0u8; HEADER as usize];
        inner
            .read_exact_at(&mut header, 0)
            .map_err(|_| invalid(Some(path), "not a sealed file (in the clear?)"))?;
        let chunk = u64::from(u32::from_le_bytes(header[24..28].try_into().expect("4")));
        if &header[..8] != MAGIC
            || header[28..] != [0; 4]
            || !chunk.is_power_of_two()
            || !(512..=1 << 20).contains(&chunk)
        {
            return Err(invalid(Some(path), "not a sealed file (in the clear?)"));
        }
        if mode != OpenMode::ReadOnly {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{}: a sealed file is written once", path.display()),
            ));
        }
        let generation = u32::from_le_bytes(header[8..12].try_into().expect("4"));
        let code = u32::from_le_bytes(header[12..16].try_into().expect("4"));
        let key = cipher
            .of_generation(generation)
            .ok_or_else(|| invalid(Some(path), "sealed under a key not given"))?;
        if key.code() != code {
            return Err(invalid(Some(path), "sealed with another cipher"));
        }
        let len = plain_len(disk, chunk).ok_or_else(|| invalid(Some(path), "cut short"))?;
        Ok(Self {
            inner,
            key,
            header,
            chunk,
            state: Mutex::new(State {
                len,
                tail: None,
                cached: None,
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A chunk on disk: its tag and its bytes.
    fn sealed(&self) -> u64 {
        self.chunk + 16
    }

    fn nonce_aad(&self, index: u64, last: bool) -> io::Result<([u8; 12], [u8; 41])> {
        let i = u32::try_from(index).map_err(|_| invalid(None, "a sealed file of 2^32 chunks"))?;
        let mut n = [0u8; 12];
        n[..8].copy_from_slice(&self.header[16..24]);
        n[8..].copy_from_slice(&i.to_le_bytes());
        let mut a = [0u8; 41];
        a[..32].copy_from_slice(&self.header);
        a[32..40].copy_from_slice(&index.to_le_bytes());
        a[40] = u8::from(last);
        Ok((n, a))
    }

    /// Chunk `index`, `bytes`, sealed onto the end of `out`.
    fn seal_chunk(
        &self,
        index: u64,
        bytes: &[u8],
        last: bool,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        let (n, a) = self.nonce_aad(index, last)?;
        let at = out.len();
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(bytes);
        let tag = self.key.aead().seal_in_place(&n, &a, &mut out[at + 16..]);
        out[at..at + 16].copy_from_slice(&tag);
        Ok(())
    }

    fn write_chunk(&self, index: u64, bytes: &[u8], last: bool) -> io::Result<()> {
        let mut out = Vec::with_capacity(16 + bytes.len());
        self.seal_chunk(index, bytes, last, &mut out)?;
        self.inner
            .write_all_at(&out, HEADER + index * self.sealed())
    }

    /// Chunks `first..=last` of a file `len` bytes long, read at once and
    /// opened in place: their bytes, one after another.
    fn read_chunks(&self, first: u64, last: u64, len: u64) -> io::Result<Vec<u8>> {
        let (c, sealed) = (self.chunk, self.sealed());
        let chunks = len.div_ceil(c).max(1);
        let end = if last + 1 == chunks {
            HEADER + last * sealed + 16 + (len - last * c)
        } else {
            HEADER + (last + 1) * sealed
        };
        let from = HEADER + first * sealed;
        let mut buf = vec![0u8; (end - from) as usize];
        self.inner
            .read_exact_at(&mut buf, from)
            .map_err(|_| invalid(None, "a sealed file ends early"))?;
        // Each chunk's bytes moved down over the tags before them.
        let mut to = 0;
        for i in first..=last {
            let at = ((i - first) * sealed) as usize;
            let body = at + 16..(at + sealed as usize).min(buf.len());
            let tag: [u8; 16] = buf[at..at + 16].try_into().expect("16");
            let (n, a) = self.nonce_aad(i, i + 1 == chunks)?;
            if !self
                .key
                .aead()
                .open_in_place(&n, &a, &mut buf[body.clone()], &tag)
            {
                return Err(invalid(
                    None,
                    "a sealed file's chunk does not open: damaged, moved, or sealed under another key",
                ));
            }
            let n = body.len();
            buf.copy_within(body, to);
            to += n;
        }
        buf.truncate(to);
        Ok(buf)
    }
}

impl File for SealedFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let (len, cached, tail) = {
            let s = self.state();
            (s.len, s.cached.clone(), s.tail.clone())
        };
        if offset >= len || buf.is_empty() {
            return Ok(0);
        }
        let end = len.min(offset + buf.len() as u64);
        let c = self.chunk;
        let (mut first, last) = (offset / c, (end - 1) / c);
        let mut at = offset;
        let mut put = |at: &mut u64, from: u64, bytes: &[u8]| {
            let s = (*at - from) as usize;
            let n = (bytes.len() - s).min((end - *at) as usize);
            let to = (*at - offset) as usize;
            buf[to..to + n].copy_from_slice(&bytes[s..s + n]);
            *at += n as u64;
        };
        // The chunks opened last, if the read begins in them.
        if let Some((i, chunks)) = &cached
            && (*i..*i + (chunks.len() as u64).div_ceil(c)).contains(&first)
        {
            put(&mut at, *i * c, chunks);
            first = at.div_ceil(c).max(first + 1);
        }
        // A writer's last chunk is in memory.
        let tail_at = (len - 1) / c;
        let disk_last = if tail.is_some() && last == tail_at {
            last.checked_sub(1)
        } else {
            Some(last)
        };
        if let Some(disk_last) = disk_last
            && first <= disk_last
        {
            let bytes = self.read_chunks(first, disk_last, len)?;
            put(&mut at, first * c, &bytes);
            self.state().cached = Some((first, Arc::new(bytes)));
        }
        if let Some(t) = &tail
            && at < end
        {
            put(&mut at, tail_at * c, t);
        }
        debug_assert_eq!(at, end);
        Ok((end - offset) as usize)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        let mut s = self.state();
        let len = s.len;
        let Some(tail) = s.tail.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a sealed file opened to read",
            ));
        };
        if offset != len {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a sealed file is written in order, once",
            ));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        // The tail is chunk `first`; full chunks before the last sealed
        // as not last.
        let c = self.chunk as usize;
        let mut first = if len == 0 { 0 } else { (len - 1) / self.chunk };
        tail.extend_from_slice(buf);
        let at = HEADER + first * self.sealed();
        let mut out = Vec::with_capacity(tail.len() + tail.len() / c * 16 + 16);
        let mut full = 0;
        while tail.len() - full > c {
            self.seal_chunk(first, &tail[full..full + c], false, &mut out)?;
            full += c;
            first += 1;
        }
        tail.drain(..full);
        self.seal_chunk(first, tail, true, &mut out)?;
        self.inner.write_all_at(&out, at)?;
        s.len += buf.len() as u64;
        s.cached = None;
        Ok(buf.len())
    }

    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }

    fn sync_order(&self) -> io::Result<()> {
        self.inner.sync_order()
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.state().len)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        if len == self.state().len {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a sealed file's length is what was written",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleus_platform::sim::SimPlatform;

    fn platform() -> Arc<dyn Platform> {
        Arc::new(SimPlatform::new(7))
    }

    fn write(fs: &dyn FileSystem, path: &Path, parts: &[&[u8]]) {
        let f = fs.open(path, OpenMode::CreateNew).unwrap();
        let mut at = 0;
        for p in parts {
            f.write_all_at(p, at).unwrap();
            at += p.len() as u64;
        }
        f.sync().unwrap();
    }

    fn read(fs: &dyn FileSystem, path: &Path) -> io::Result<Vec<u8>> {
        let f = fs.open(path, OpenMode::ReadOnly)?;
        let mut v = vec![0; f.size()? as usize];
        f.read_exact_at(&mut v, 0)?;
        Ok(v)
    }

    fn bytes(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + seed as usize) as u8).collect()
    }

    /// Files of every length about a chunk's edges, written in pieces of
    /// every size, read back whole and piece by piece; nothing of them
    /// shows on disk.
    #[test]
    fn what_is_written_reads_back_and_does_not_show() {
        let p = platform();
        p.fs().create_dir_all(Path::new("/e")).unwrap();
        let fs = SealedFs::new(Arc::clone(&p), Some(PageCipher::new([3; 32])));
        for len in [
            0,
            1,
            100,
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
            2 * CHUNK,
            3 * CHUNK + 77,
        ] {
            for piece in [1, 7, 1000, CHUNK, 3 * CHUNK] {
                let data = bytes(len, len as u8);
                let path = Path::new("/e/f");
                if p.fs().exists(path).unwrap() {
                    p.fs().remove_file(path).unwrap();
                }
                let parts: Vec<&[u8]> = data.chunks(piece).collect();
                write(&fs, path, &parts);
                assert_eq!(
                    read(&fs, path).unwrap(),
                    data,
                    "len {len}, pieces of {piece}"
                );
                let f = fs.open(path, OpenMode::ReadOnly).unwrap();
                for at in (0..len).step_by(997) {
                    let mut b = vec![0; 1500.min(len - at)];
                    f.read_exact_at(&mut b, at as u64).unwrap();
                    assert_eq!(b, data[at..at + b.len()]);
                }
                let raw = read(p.fs(), path).unwrap();
                if len >= 64 {
                    assert!(
                        !raw.windows(16)
                            .any(|w| data.windows(16).take(64).any(|d| d == w)),
                        "plain bytes show on disk"
                    );
                }
            }
        }
    }

    /// Damaged, cut short at a chunk's end, a chunk moved, the header
    /// changed, under another key, in the clear: it does not open.
    #[test]
    fn tampered_files_do_not_open() {
        let p = platform();
        let raw = p.fs();
        raw.create_dir_all(Path::new("/e")).unwrap();
        let fs = SealedFs::new(Arc::clone(&p), Some(PageCipher::new([3; 32])));
        let data = bytes(3 * CHUNK + 10, 1);
        write(&fs, Path::new("/e/good"), &[&data]);
        let good = read(raw, Path::new("/e/good")).unwrap();
        let put = |name: &str, b: &[u8]| {
            let path = Path::new("/e").join(name);
            let f = raw.open(&path, OpenMode::CreateNew).unwrap();
            f.write_all_at(b, 0).unwrap();
            path
        };
        let mut flipped = good.clone();
        flipped[HEADER as usize + 100] ^= 1;
        assert!(read(&fs, &put("flipped", &flipped)).is_err(), "damaged");
        let cut = &good[..(HEADER + 2 * SEALED) as usize];
        assert!(read(&fs, &put("cut", cut)).is_err(), "cut at a chunk's end");
        let mut swapped = good.clone();
        let (a, b) = (HEADER as usize, (HEADER + SEALED) as usize);
        let first: Vec<u8> = swapped[a..b].to_vec();
        swapped.copy_within(b..b + SEALED as usize, a);
        swapped[b..b + SEALED as usize].copy_from_slice(&first);
        assert!(
            read(&fs, &put("swapped", &swapped)).is_err(),
            "chunks moved"
        );
        let mut head = good.clone();
        head[20] ^= 1;
        assert!(read(&fs, &put("head", &head)).is_err(), "header changed");
        let other = SealedFs::new(Arc::clone(&p), Some(PageCipher::new([4; 32])));
        assert!(read(&other, Path::new("/e/good")).is_err(), "another key");
        assert!(read(&fs, &put("clear", &data)).is_err(), "in the clear");
        assert_eq!(read(&fs, Path::new("/e/good")).unwrap(), data);
    }

    /// A new key: new files under it, old ones still open (by the key
    /// before, kept beside it) until sealed again; then the old key is
    /// not needed.
    #[test]
    fn a_new_key_seals_files_again() {
        let p = platform();
        p.fs().create_dir_all(Path::new("/e")).unwrap();
        let old = PageCipher::new([3; 32]);
        let fs = SealedFs::new(Arc::clone(&p), Some(old.clone()));
        let data = bytes(2 * CHUNK + 5, 9);
        let path = Path::new("/e/f");
        write(&fs, path, &[&data]);
        let opened = fs.open(path, OpenMode::ReadOnly).unwrap();
        let new = PageCipher::new([5; 32])
            .with_generation(1)
            .with_previous(old);
        fs.set_cipher(Some(new.clone()));
        assert_eq!(fs.generation_of(path).unwrap(), 0);
        assert_eq!(read(&fs, path).unwrap(), data);
        assert!(fs.reseal(path).unwrap());
        assert!(!fs.reseal(path).unwrap(), "once");
        assert_eq!(fs.generation_of(path).unwrap(), 1);
        fs.set_cipher(Some(new.without_previous()));
        assert_eq!(read(&fs, path).unwrap(), data);
        let mut b = vec![0; data.len()];
        opened.read_exact_at(&mut b, 0).unwrap();
        assert_eq!(b, data, "a file open before reads what it was opened on");
    }
}
