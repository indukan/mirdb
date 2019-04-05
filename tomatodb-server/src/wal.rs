use std::borrow::Borrow;
use std::cmp::min;
use std::fmt::Debug;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::remove_file;
use std::io::Cursor;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::marker::PhantomData;
use std::num::Wrapping;
use std::path::Path;
use std::path::PathBuf;

use bincode::deserialize_from;
use bincode::serialize;
use glob::glob;
use integer_encoding::{VarIntReader, VarIntWriter};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;

use crate::error::MyResult;
use crate::options::Options;
use crate::utils::make_file_name;
use sstable::TableReader;
use sstable::TableBuilder;

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct LogEntry<K, V> {
    k: K,
    v: Option<V>,
}

impl<K, V> LogEntry<K, V> {
    pub fn new(k: K, v: Option<V>) -> Self {
        LogEntry { k, v }
    }

    pub fn key(&self) -> &K {
        &self.k
    }

    pub fn value(&self) -> &Option<V> {
        &self.v
    }

    pub fn kv(self) -> (K, Option<V>) {
        (self.k, self.v)
    }
}

pub struct WALSeg<K, V> {
    file: File,
    path: PathBuf,
    deleted_: bool,
    k: PhantomData<K>,
    v: PhantomData<V>,
}

impl<K: Serialize, V: Serialize> WALSeg<K, V> {
    pub fn new<T: AsRef<Path>>(path: T) -> MyResult<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path.as_ref())?;

        Ok(WALSeg {
            file,
            path: path.as_ref().to_path_buf(),
            deleted_: false,
            k: PhantomData,
            v: PhantomData,
        })
    }

    pub fn deleted(&self) -> bool {
        self.deleted_
    }

    pub fn iter(&self) -> MyResult<WALSegIter<K, V>> {
        WALSegIter::new(&self.path)
    }

    pub fn file_size(&self) -> MyResult<usize> {
        Ok(self.file.metadata()?.len() as usize)
    }

    pub fn append(&mut self, entry: &LogEntry<K, V>) -> MyResult<()> {
        let buf = serialize(entry)?;
        self.file.write_varint(buf.len())?;
        self.file.write(&buf)?;
        self.file.sync_data()?;
        Ok(())
    }

    pub fn delete(&mut self) -> MyResult<()> {
        remove_file(&self.path)?;
        self.deleted_ = true;
        Ok(())
    }
}

impl<V: Serialize + DeserializeOwned> WALSeg<Vec<u8>, V> {
    pub fn build_sstable(&self, opt: Options, path: &Path) -> MyResult<(String, TableReader)> {
        let table_opt = opt.to_table_opt();
        let mut tb = TableBuilder::new(&path, table_opt.clone())?;
        for entry in self.iter()? {
            tb.add(&entry.k, &serialize(&entry.v)?)?;
        }
        tb.flush()?;
        Ok((path.to_str().unwrap().to_owned(), TableReader::new(path, table_opt.clone())?))
    }
}

pub struct WALSegIter<K, V> {
    file: File,
    offset: usize,
    k: PhantomData<K>,
    v: PhantomData<V>,
}

impl<K, V> WALSegIter<K, V> {
    pub fn new<T: AsRef<Path>>(path: T) -> MyResult<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)?;

        Ok(WALSegIter {
            file,
            offset: 0,
            k: PhantomData,
            v: PhantomData,
        })
    }

    pub fn file_size(&self) -> MyResult<usize> {
        Ok(self.file.metadata()?.len() as usize)
    }
}

impl<K: DeserializeOwned, V: DeserializeOwned> Iterator for WALSegIter<K, V> {
    type Item = LogEntry<K, V>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.file_size().expect("wal file size error") {
            return None;
        }
        self.file.seek(SeekFrom::Start(self.offset as u64)).expect("seek wal file error");
        let size = self.file.read_varint().expect("read varint from wal file error");
        let offset = self.file.seek(SeekFrom::Current(0)).expect("seek wal file current offset error") as usize;
        let mut data = Vec::with_capacity(size);
        let mut buf = [0; 512];
        while data.len() < size {
            let remain = size - data.len();
            let size = self.file.read(&mut buf).expect("read data from wal file error");
            if size == 0 {
                break;
            }
            data.extend_from_slice(&buf[..min(remain, size)]);
        }
        let size = data.len();
        let cursor = Cursor::new(data);
        let entry: LogEntry<K, V> = deserialize_from(cursor).expect("deserialize from wal file error");
        self.offset = offset + size;
        Some(entry)
    }
}

pub struct WAL<K, V> {
    opt: Options,
    pub segs: Vec<WALSeg<K, V>>,
    current_file_num: usize,
}

impl<K: Serialize, V: Serialize> WAL<K, V> {
    pub fn new(opt: Options) -> MyResult<Self> {
        let path = Path::new(&opt.work_dir);
        let mut paths = vec![];
        for entry in glob(path.join("*.wal").to_str().expect("path to str"))? {
            match entry {
                Ok(path) => paths.push(path),
                _ => (),
            }
        }
        paths.sort();
        let segs = paths.iter().map(|p| WALSeg::new(&p.as_path()).expect("new walseg")).collect();
        Ok(WAL {
            opt,
            segs,
            current_file_num: 0,
        })
    }

    pub fn seg_count(&self) -> usize {
        self.segs.len()
    }

    pub fn get_seg(&self, i: usize) -> Option<&WALSeg<K, V>> {
        self.segs.get(i)
    }

    pub fn get_seg_mut(&mut self, i: usize) -> Option<&mut WALSeg<K, V>> {
        self.segs.get_mut(i)
    }

    pub fn append(&mut self, entry: &LogEntry<K, V>) -> MyResult<()> {
        let l = self.segs.len();
        if l == 0 {
            self.new_seg()?;
        }
        let l = self.segs.len();
        self.segs[l - 1].append(entry)
    }

    pub fn truncate(&mut self, n: usize) -> MyResult<()> {
        for _ in 0..n {
            self.consume_seg()?;
        }
        Ok(())
    }

    pub fn consume_seg(&mut self) -> MyResult<()> {
        let mut i = 0;
        while i < self.segs.len() {
            let seg = &self.segs[i];
            if !seg.deleted() {
                break;
            }
            i += 1;
        }

        if i >= self.segs.len() {
            return Ok(());
        }

        self.segs[i].delete()
    }

    pub fn new_seg(&mut self) -> MyResult<()> {
        let file_num = self.new_file_num();
        let file_name = make_file_name(file_num, "wal");
        let path = Path::new(&self.opt.work_dir);
        let path = path.join(file_name);
        let seg = WALSeg::new(path.as_path())?;
        self.segs.push(seg);
        Ok(())
    }

    fn new_file_num(&mut self) -> usize {
        let n = self.current_file_num;
        self.current_file_num += 1;
        n
    }

    pub fn iter(&self) -> MyResult<WALIter<K, V>> {
        Ok(WALIter::new(&self))
    }
}

pub struct WALIter<'a, K, V> {
    wal: &'a WAL<K, V>,
    index: usize,
    seg_iter: Option<WALSegIter<K, V>>
}

impl<'a, K, V> WALIter<'a, K, V> {
    pub fn new(wal: &'a WAL<K, V>) -> Self {
        WALIter {
            wal,
            index: 0,
            seg_iter: None
        }
    }
}

impl<'a, K: Serialize + DeserializeOwned, V: Serialize + DeserializeOwned> Iterator for WALIter<'a, K, V> {
    type Item = LogEntry<K, V>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(seg_iter) = &mut self.seg_iter {
            let n = seg_iter.next();
            if n.is_some() {
                return n;
            } else {
                self.index += 1;
            }
        }
        while self.index < self.wal.seg_count() {
            let seg = &self.wal.get_seg(self.index).expect("get seg");
            if !seg.deleted() {
                break;
            }
            self.index += 1;
        }
        if self.index >= self.wal.seg_count() {
            return None;
        }
        self.seg_iter = Some(self.wal.segs[self.index].iter().expect("get walseg iter"));
        self.next()
    }
}

#[cfg(test)]
mod test {
    use crate::test_utils::get_test_opt;

    use super::*;

    #[test]
    fn test_wal_seg() -> MyResult<()> {
        let p = Path::new("/tmp/wal");
        let mut seg = WALSeg::new(&p)?;
        let mut kvs = Vec::with_capacity(3);
        kvs.push((b"a".to_vec(), b"abcasldkfjaoiwejfawoejfoaisjdflaskdjfoias".to_vec()));
        kvs.push((b"b".to_vec(), b"bbcasdlfjasldfj".to_vec()));
        kvs.push((b"c".to_vec(), b"cbcasldfjowiejfoaisdjfalskdfj".to_vec()));
        for (k, v) in &kvs {
            let entry = LogEntry::new(k.clone(), Some(v.clone()));
            seg.append(&entry)?;
        }
        let mut iter = seg.iter()?;
        for (k, v) in &kvs {
            let entry = LogEntry::new(k.clone(), Some(v.clone()));
            assert_eq!(Some(entry), iter.next());
        }
        assert_eq!(None, iter.next());
        Ok(())
    }

    #[test]
    fn test_wal() -> MyResult<()> {
        let opt = get_test_opt();
        let mut wal = WAL::new(opt.clone())?;
        let mut kvs = Vec::with_capacity(3);
        kvs.push((b"a".to_vec(), b"abcasldkfjaoiwejfawoejfoaisjdflaskdjfoias".to_vec()));
        kvs.push((b"b".to_vec(), b"bbcasdlfjasldfj".to_vec()));
        kvs.push((b"c".to_vec(), b"cbcasldfjowiejfoaisdjfalskdfj".to_vec()));
        for (k, v) in &kvs {
            let entry = LogEntry::new(k.clone(), Some(v.clone()));
            wal.new_seg()?;
            wal.append(&entry)?;
        }
        let mut wal = WAL::new(opt.clone())?;
        let mut iter = wal.iter()?;
        for (k, v) in &kvs {
            let entry = LogEntry::new(k.clone(), Some(v.clone()));
            assert_eq!(Some(entry), iter.next());
        }
        assert_eq!(None, iter.next());
        wal.truncate(1)?;
        let mut iter = wal.iter()?;
        for (i, (k, v)) in kvs.iter().enumerate() {
            if i == 0 {
                continue;
            }
            let entry = LogEntry::new(k.clone(), Some(v.clone()));
            assert_eq!(Some(entry), iter.next());
        }
        assert_eq!(None, iter.next());
        wal.truncate(1)?;
        let wal = WAL::new(opt.clone())?;
        let mut iter = wal.iter()?;
        for (i, (k, v)) in kvs.iter().enumerate() {
            if i <= 1 {
                continue;
            }
            let entry = LogEntry::new(k.clone(), Some(v.clone()));
            assert_eq!(Some(entry), iter.next());
        }
        assert_eq!(None, iter.next());
        Ok(())
    }
}