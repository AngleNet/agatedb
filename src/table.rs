pub(crate) mod builder;
mod iterator;

use crate::checksum;
use crate::opt::Options;
use crate::Error;
use crate::Result;
use bytes::{Buf, Bytes};
use iterator::{Iterator as TableIterator, ITERATOR_NOCACHE, ITERATOR_REVERSED};
use prost::Message;
use proto::meta::{BlockOffset, Checksum, TableIndex};
use std::fs;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

/// MmapFile stores SST data. `File` refers to a file on disk,
/// and `Memory` refers to data in memory.
// TODO: use a mmap library instead of handling I/O on our own
enum MmapFile {
    File {
        name: PathBuf,
        // TODO: remove this mutex and allow multi-thread read
        file: Mutex<fs::File>,
    },
    Memory {
        data: Bytes,
    },
}

impl MmapFile {
    /// Returns if data is in memory.
    pub fn is_in_memory(&self) -> bool {
        match self {
            Self::File { .. } => false,
            Self::Memory { .. } => true,
        }
    }
}

/// TableInner stores data of an SST.
/// It is immutable once created and initialized.
pub struct TableInner {
    /// file struct of SST
    file: MmapFile,
    /// size of SST
    table_size: usize,
    /// smallest key
    smallest: Bytes,
    /// biggest key
    biggest: Bytes,
    /// SST id
    id: u64,
    /// checksum of SST
    checksum: Bytes,
    /// estimated size, only used on encryption or compression
    estimated_size: u32,
    /// index of SST
    index: TableIndex,
    /// start position of index
    index_start: usize,
    /// length of index
    index_len: usize,
    /// table options
    opt: Options,
}

pub struct Table {
    inner: Arc<TableInner>,
}

/// `AsRef<TableInner>` is only used in `init_biggest_and_smallest`
/// to construct a table iterator from `&TableInner`.
impl AsRef<TableInner> for TableInner {
    fn as_ref(&self) -> &TableInner {
        self
    }
}

impl TableInner {
    /// Create an SST from bytes data generated with table builder
    fn create(path: &Path, data: Bytes, opt: Options) -> Result<TableInner> {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)?;
        f.write(&data)?;
        // TODO: pass file object directly to open
        drop(f);
        Self::open(path, opt)
    }

    /// Open an existing SST on disk
    fn open(path: &Path, opt: Options) -> Result<TableInner> {
        let f = fs::OpenOptions::new()
            .read(true)
            .write(false)
            .create(false)
            .open(path)?;
        let file_name = path.file_name().unwrap().to_str().unwrap();
        let id = parse_file_id(file_name)?;
        let meta = f.metadata()?;
        let table_size = meta.len();
        let mut inner = TableInner {
            file: MmapFile::File {
                file: Mutex::new(f),
                name: path.to_path_buf(),
            },
            table_size: table_size as usize,
            smallest: Bytes::new(),
            biggest: Bytes::new(),
            id,
            checksum: Bytes::new(),
            estimated_size: 0,
            index: TableIndex::default(),
            index_start: 0,
            index_len: 0,
            opt,
        };
        inner.init_biggest_and_smallest()?;
        // TODO: verify checksum
        Ok(inner)
    }

    /// Open an existing SST from data in memory
    fn open_in_memory(data: Bytes, id: u64, opt: Options) -> Result<TableInner> {
        let table_size = data.len();
        let mut inner = TableInner {
            file: MmapFile::Memory { data },
            opt,
            table_size,
            id,
            smallest: Bytes::new(),
            biggest: Bytes::new(),
            checksum: Bytes::new(),
            estimated_size: 0,
            index: TableIndex::default(),
            index_start: 0,
            index_len: 0,
        };
        inner.init_biggest_and_smallest()?;
        Ok(inner)
    }

    fn init_biggest_and_smallest(&mut self) -> Result<()> {
        let ko = self.init_index()?;
        self.smallest = Bytes::copy_from_slice(&ko.key);
        let mut it = TableIterator::new(&self, ITERATOR_REVERSED | ITERATOR_NOCACHE);
        it.rewind();
        if !it.valid() {
            return Err(Error::TableRead(format!(
                "failed to initialize biggest for table {}",
                self.filename()
            )));
        }
        self.biggest = Bytes::copy_from_slice(it.key());
        Ok(())
    }

    fn init_index(&mut self) -> Result<&BlockOffset> {
        let mut read_pos = self.table_size;

        // read checksum length from last 4 bytes
        read_pos -= 4;
        let mut buf = self.read(read_pos, 4)?;
        let checksum_len = buf.get_u32() as usize;
        if (checksum_len as i32) < 0 {
            return Err(Error::TableRead(
                "checksum length less than zero".to_string(),
            ));
        }

        // read checksum
        read_pos -= checksum_len;
        let buf = self.read(read_pos, checksum_len)?;
        let chksum = Checksum::decode(buf)?;

        // read index size from footer
        read_pos -= 4;
        let mut buf = self.read(read_pos, 4)?;
        self.index_len = buf.get_u32() as usize;

        // read index
        read_pos -= self.index_len;
        self.index_start = read_pos;
        let data = self.read(read_pos, self.index_len)?;
        checksum::verify_checksum(&data, &chksum)?;

        self.index = self.read_table_index()?;

        // TODO: compression
        self.estimated_size = self.table_size as u32;

        // TODO: has bloom filter

        Ok(&self.index.offsets[0])
    }

    fn key_splits(&mut self, _n: usize, _prefix: Bytes) -> Vec<String> {
        // if n == 0 {
        //     return vec![];
        // }

        // let output_len = self.offsets_length();
        // let jump = output_len / n;
        // let _jump = if jump == 0 { 1 } else { jump };

        // let _block_offset = BlockOffset::default();
        // let res = vec![];

        unimplemented!()
    }

    fn fetch_index(&self) -> &TableIndex {
        return &self.index;
        // TODO: encryption
    }

    fn offsets_length(&self) -> usize {
        self.fetch_index().offsets.len()
    }

    fn offsets(&self, idx: usize) -> Option<&BlockOffset> {
        self.fetch_index().offsets.get(idx)
    }

    fn block(&self, idx: usize, _use_cache: bool) -> Result<Arc<Block>> {
        // TODO: support cache
        if idx >= self.offsets_length() {
            return Err(Error::TableRead("block out of index".to_string()));
        }
        let block_offset = self.offsets(idx).ok_or(Error::TableRead(format!(
            "failed to get offset block {}",
            idx
        )))?;

        let offset = block_offset.offset as usize;
        let data = self.read(offset, block_offset.len as usize)?;

        let mut read_pos = data.len() - 4; // first read checksum length
        let checksum_len = (&data[read_pos..read_pos + 4]).get_u32() as usize;

        if checksum_len > data.len() {
            return Err(Error::TableRead("invalid checksum length".to_string()));
        }

        // read checksum
        read_pos -= checksum_len;
        let checksum = data.slice(read_pos..read_pos + checksum_len);

        // read num entries
        read_pos -= 4;
        let num_entries = (&data[read_pos..read_pos + 4]).get_u32() as usize;

        let entries_index_start = read_pos - num_entries * 4;
        let entries_index_end = entries_index_start + num_entries * 4;

        let mut entry_offsets_ptr = &data[entries_index_start..entries_index_end];
        let mut entry_offsets = Vec::with_capacity(num_entries);
        for _ in 0..num_entries {
            entry_offsets.push(entry_offsets_ptr.get_u32_le());
        }

        Ok(Arc::new(Block {
            offset,
            entries_index_start,
            data: data.slice(..read_pos + 4),
            entry_offsets,
            checksum_len,
            checksum,
        }))
    }

    fn index_key(&self) -> u64 {
        self.id
    }

    /// Get number of keys in SST
    pub fn key_count(&self) -> u32 {
        self.fetch_index().key_count
    }

    /// Get size of index
    pub fn index_size(&self) -> usize {
        self.index_len
    }

    /// Get size of bloom filter
    pub fn bloom_filter_size(&self) -> usize {
        self.fetch_index().bloom_filter.len()
    }

    /// Get size of SST
    pub fn size(&self) -> u64 {
        self.table_size as u64
    }

    /// Get smallest key of current table
    pub fn smallest(&self) -> &Bytes {
        &self.smallest
    }

    /// Get biggest key of current table
    pub fn biggest(&self) -> &Bytes {
        &self.biggest
    }

    /// Get filename of current SST. Returns `<memtable>` if in-memory.
    pub fn filename(&self) -> String {
        match &self.file {
            MmapFile::Memory { .. } => "<memtable>".to_string(),
            MmapFile::File { name, .. } => name.to_string_lossy().into_owned(),
        }
    }

    /// Get SST id
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Check if the table doesn't contain an entry with bloom filter.
    /// Always return false if no bloom filter is present in SST.
    pub fn does_not_have(_hash: u32) -> bool {
        false
        // TODO: add bloom filter
    }

    fn read_bloom_filter(&self) {
        unimplemented!()
    }

    pub(crate) fn read_table_index(&self) -> Result<TableIndex> {
        let data = self.read(self.index_start, self.index_len)?;
        // TODO: prefetch
        let result = Message::decode(data)?;
        Ok(result)
    }

    fn verify_checksum(&self) -> Result<()> {
        let table_index = self.fetch_index();
        for i in 0..table_index.offsets.len() {
            let block = self.block(i, true)?;
            // TODO: table opts
            block.verify_checksum()?;
        }
        Ok(())
    }

    fn read(&self, offset: usize, size: usize) -> Result<Bytes> {
        self.bytes(offset, size)
    }

    fn bytes(&self, offset: usize, size: usize) -> Result<Bytes> {
        match &self.file {
            MmapFile::Memory { data } => {
                if offset + size > data.len() {
                    Err(Error::TableRead(format!(
                        "out of range, offset={}, size={}, len={}",
                        offset,
                        size,
                        data.len()
                    )))
                } else {
                    Ok(data.slice(offset..offset + size))
                }
            }
            MmapFile::File { file, .. } => {
                // TODO: use MmapFile
                let mut file = file.lock().unwrap();
                file.seek(std::io::SeekFrom::Start(offset as u64))?;
                let mut buf = vec![0; size];
                file.read_exact(&mut buf)?;
                assert_eq!(buf.len(), size);
                Ok(Bytes::from(buf))
            }
        }
    }

    fn is_in_memory(&self) -> bool {
        self.file.is_in_memory()
    }

    fn max_version(&self) -> u64 {
        unimplemented!()
        // self.fetch_index()?.max_version()
    }
}

/*
impl Drop for TableInner {
    fn drop(&mut self) {
        let f = match self.file.take() {
            Some(f) => f,
            None => return,
        };
        f.file.set_len(0).unwrap();
        drop(f.file);
        fs::remove_file(&f.path).unwrap();
    }
}
*/

/// Block contains several entries. It can be obtained from an SST.
#[derive(Default)]
pub struct Block {
    offset: usize,
    data: Bytes,
    checksum: Bytes,
    entries_index_start: usize,
    entry_offsets: Vec<u32>,
    checksum_len: usize,
}

impl Block {
    fn size(&self) -> u64 {
        3 * std::mem::size_of::<usize>() as u64
            + self.data.len() as u64
            + self.checksum.len() as u64
            + self.entry_offsets.len() as u64 * std::mem::size_of::<u32>() as u64
    }

    fn verify_checksum(&self) -> Result<()> {
        let chksum = prost::Message::decode(self.data.clone())?;
        checksum::verify_checksum(&self.data, &chksum)
    }
}

fn parse_file_id(name: &str) -> Result<u64> {
    if !name.ends_with(".sst") {
        return Err(Error::InvalidFilename(name.to_string()));
    }
    match name[..name.len() - 4].parse() {
        Ok(id) => Ok(id),
        Err(_) => Err(Error::InvalidFilename(name.to_string())),
    }
}

impl Table {
    /// Create an SST from bytes data generated with table builder
    pub fn create(path: &Path, data: Bytes, opt: Options) -> Result<Table> {
        Ok(Table {
            inner: Arc::new(TableInner::create(path, data, opt)?),
        })
    }

    /// Open an existing SST on disk
    pub fn open(path: &Path, opt: Options) -> Result<Table> {
        Ok(Table {
            inner: Arc::new(TableInner::open(path, opt)?),
        })
    }

    /// Open an existing SST from data in memory
    pub fn open_in_memory(data: Bytes, id: u64, opt: Options) -> Result<Table> {
        Ok(Table {
            inner: Arc::new(TableInner::open_in_memory(data, id, opt)?),
        })
    }

    /// Get block numbers
    pub(crate) fn offsets_length(&self) -> usize {
        self.inner.offsets_length()
    }

    /// Get all block offsets
    pub(crate) fn offsets(&self, idx: usize) -> Option<&BlockOffset> {
        self.inner.offsets(idx)
    }

    /// Get one block from table
    pub(crate) fn block(&self, block_pos: usize, use_cache: bool) -> Result<Arc<Block>> {
        self.inner.block(block_pos, use_cache)
    }

    /// Get an iterator to this table
    pub fn new_iterator(&self, opt: usize) -> TableIterator<Arc<TableInner>> {
        TableIterator::new(self.inner.clone(), opt)
    }

    /// Get max version of this table
    pub fn max_version(&self) -> u64 {
        self.inner.max_version()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{key_with_ts, user_key};
    use crate::value::Value;
    use builder::Builder;
    use tempdir::TempDir;

    fn key(prefix: &[u8], i: usize) -> Bytes {
        Bytes::from([prefix, format!("{:04}", i).as_bytes()].concat())
    }

    fn key_isize(prefix: &[u8], i: isize) -> Bytes {
        Bytes::from([prefix, format!("{:04}", i).as_bytes()].concat())
    }

    #[test]
    fn test_generate_key() {
        assert_eq!(key(b"key", 233), Bytes::from("key0233"));
    }

    fn get_test_table_options() -> Options {
        Options {
            block_size: 4 * 1024,
            table_size: 0,
            bloom_false_positive: 0.01,
        }
    }

    fn build_test_table(prefix: &[u8], n: usize, mut opts: Options) -> Table {
        if opts.block_size == 0 {
            opts.block_size = 4 * 1024;
        }
        assert!(n <= 10000);

        let mut kv_pairs = vec![];

        for i in 0..n {
            let k = key(prefix, i);
            let v = Bytes::from(i.to_string());
            kv_pairs.push((k, v));
        }

        build_table(kv_pairs, opts)
    }

    fn build_table(mut kv_pairs: Vec<(Bytes, Bytes)>, opts: Options) -> Table {
        let mut builder = Builder::new(opts.clone());
        let tmp_dir = TempDir::new("agatedb").unwrap();
        let filename = tmp_dir.path().join("1.sst".to_string());

        kv_pairs.sort_by(|x, y| x.0.cmp(&y.0));

        for (k, v) in kv_pairs {
            builder.add(&key_with_ts(&k[..], 0), Value::new_with_meta(v, b'A', 0), 0);
        }
        let data = builder.finish();

        Table::create(&filename, data, opts).unwrap()
        // you can also test in-memory table
        // Table::open_in_memory(data, 233, opts).unwrap()
        // `tmp_dir` will be dropped and the temp folder will be deleted
        // when we return from this function. However, as we saves file
        // descriptor to the file, we could still safely access that file.
    }

    #[test]
    fn test_table_iterator() {
        for n in 99..=101 {
            let opts = get_test_table_options();
            let table = build_test_table(b"key", n, opts);
            let mut it = table.new_iterator(0);
            it.rewind();
            let mut count = 0;
            while it.valid() {
                let v = it.value();
                let k = it.key();
                assert_eq!(count.to_string(), v.value);
                assert_eq!(key_with_ts(&key(b"key", count)[..], 0), k);
                count += 1;
                it.next();
            }
            assert_eq!(count, n);
        }
    }

    #[test]
    fn test_seek_to_first() {
        for n in vec![99, 100, 101, 199, 200, 250, 9999, 10000] {
            let opts = get_test_table_options();
            let table = build_test_table(b"key", n, opts);
            let mut it = table.new_iterator(0);
            it.seek_to_first();
            assert!(it.valid());
            assert_eq!(it.value().value, "0");
            assert_eq!(it.value().meta, b'A');
        }
    }

    #[test]
    fn test_seek_to_last() {
        for n in vec![99, 100, 101, 199, 200, 250, 9999, 10000] {
            let opts = get_test_table_options();
            let table = build_test_table(b"key", n, opts);
            let mut it = table.new_iterator(0);
            it.seek_to_last();
            assert!(it.valid());
            assert_eq!(it.value().value, (n - 1).to_string());
            assert_eq!(it.value().meta, b'A');
            it.prev();
            assert!(it.valid());
            assert_eq!(it.value().value, (n - 2).to_string());
            assert_eq!(it.value().meta, b'A');
        }
    }

    #[test]
    fn test_seek() {
        let opts = get_test_table_options();
        let table = build_test_table(b"k", 10000, opts);
        let mut it = table.new_iterator(0);

        let data = vec![
            (b"abc".to_vec(), true, b"k0000".to_vec()),
            (b"k0100".to_vec(), true, b"k0100".to_vec()),
            (b"k0100b".to_vec(), true, b"k0101".to_vec()),
            (b"k1234".to_vec(), true, b"k1234".to_vec()),
            (b"k1234b".to_vec(), true, b"k1235".to_vec()),
            (b"k9999".to_vec(), true, b"k9999".to_vec()),
            (b"z".to_vec(), false, b"".to_vec()),
        ];

        for (input, valid, out) in data {
            it.seek(&key_with_ts(input.as_slice(), 0));
            assert_eq!(it.valid(), valid);
            if !valid {
                continue;
            }
            // compare Bytes to make output more readable
            assert_eq!(Bytes::copy_from_slice(user_key(it.key())), Bytes::from(out));
        }
    }

    #[test]
    fn test_seek_for_prev() {
        let opts = get_test_table_options();
        let table = build_test_table(b"k", 10000, opts);
        let mut it = table.new_iterator(0);

        let data = vec![
            ("abc", false, ""),
            ("k0100", true, "k0100"),
            ("k0100b", true, "k0100"), // Test case where we jump to next block.
            ("k1234", true, "k1234"),
            ("k1234b", true, "k1234"),
            ("k9999", true, "k9999"),
            ("z", true, "k9999"),
        ];

        for (input, valid, out) in data {
            it.seek_for_prev(&key_with_ts(input.as_bytes(), 0));
            assert_eq!(it.valid(), valid);
            if !valid {
                continue;
            }
            // compare Bytes to make output more readable
            assert_eq!(Bytes::copy_from_slice(user_key(it.key())), Bytes::from(out));
        }
    }

    #[test]
    fn test_iterate_from_start() {
        for n in vec![99, 100, 101, 199, 200, 250, 9999, 10000] {
            let opts = get_test_table_options();
            let table = build_test_table(b"key", n, opts);
            let mut it = table.new_iterator(0);
            it.reset();
            it.seek_to_first();
            assert!(it.valid());

            let mut count = 0;
            while it.valid() {
                let v = it.value();
                assert_eq!(count.to_string(), v.value);
                assert_eq!(b'A', v.meta);
                it.next();
                count += 1;
            }
        }
    }

    #[test]
    fn test_iterate_from_end() {
        for n in vec![99, 100, 101, 199, 200, 250, 9999, 10000] {
            let opts = get_test_table_options();
            let table = build_test_table(b"key", n, opts);
            let mut it = table.new_iterator(0);
            it.reset();
            it.seek(&key_with_ts(b"zzzzzz" as &[u8], 0));
            assert!(!it.valid());

            for i in (0..n).rev() {
                it.prev();
                assert!(it.valid());
                let v = it.value();
                assert_eq!(i.to_string(), v.value);
                assert_eq!(b'A', v.meta);
            }
            it.prev();
            assert!(!it.valid())
        }
    }

    #[test]
    fn test_table() {
        let opts = get_test_table_options();
        let table = build_test_table(b"key", 10000, opts);
        let mut it = table.new_iterator(0);
        let mut kid = 1010;
        let seek = key_with_ts(&key(b"key", kid)[..], 0);
        it.seek(&seek);
        while it.valid() {
            assert_eq!(user_key(it.key()), &key(b"key", kid)[..]);
            kid += 1;
            it.next();
        }
        assert_eq!(kid, 10000);

        it.seek(&key_with_ts(&key(b"key", 99999)[..], 0));
        assert!(!it.valid());

        it.seek(&key_with_ts(&key_isize(b"key", -1)[..], 0));
        assert!(it.valid());

        assert_eq!(user_key(it.key()), key(b"key", 0));
    }

    #[test]
    fn test_iterate_back_and_forth() {
        let opts = get_test_table_options();
        let table = build_test_table(b"key", 10000, opts);
        let mut it = table.new_iterator(0);
        let seek = key_with_ts(&key(b"key", 1010)[..], 0);

        it.seek(&seek);
        assert!(it.valid());
        assert_eq!(it.key(), &seek);

        it.prev();
        it.prev();
        assert!(it.valid());
        assert_eq!(user_key(it.key()), &key(b"key", 1008)[..]);

        it.next();
        it.next();
        assert!(it.valid());
        assert_eq!(user_key(it.key()), &key(b"key", 1010)[..]);

        it.seek(&key_with_ts(&key(b"key", 2000)[..], 0));
        assert!(it.valid());
        assert_eq!(user_key(it.key()), &key(b"key", 2000)[..]);

        it.prev();
        assert!(it.valid());
        assert_eq!(user_key(it.key()), &key(b"key", 1999)[..]);

        it.seek_to_first();
        assert!(it.valid());
        assert_eq!(user_key(it.key()), &key(b"key", 0)[..]);
    }

    #[test]
    fn test_uni_iterator() {
        let opts = get_test_table_options();
        let table = build_test_table(b"key", 10000, opts);
        {
            let mut it = table.new_iterator(0);
            it.rewind();
            let mut count = 0;
            while it.valid() {
                let v = it.value();
                assert_eq!(count.to_string(), v.value);
                assert_eq!(b'A', v.meta);
                it.next();
                count += 1;
            }
            assert_eq!(count, 10000);
        }
        {
            let mut it = table.new_iterator(ITERATOR_REVERSED);
            it.rewind();
            let mut count = 0;
            while it.valid() {
                let v = it.value();
                assert_eq!((10000 - 1 - count).to_string(), v.value);
                assert_eq!(b'A', v.meta);
                it.next();
                count += 1;
            }
            assert_eq!(count, 10000);
        }
    }

    // TODO: concat iterators and merge iterators

    fn value(i: usize) -> Bytes {
        Bytes::from(format!("{:01048576}", i)) // 1MB value
    }

    #[test]
    fn test_table_big_values() {
        let n: usize = 100;
        let opts = Options {
            block_size: 4 * 1024,
            bloom_false_positive: 0.01,
            table_size: (n as u64) * (1 << 20),
        };
        let mut builder = Builder::new(opts.clone());

        for i in 0..n {
            let key = key_with_ts(&key(b"", i)[..], i as u64 + 1);
            let vs = Value::new(value(i));
            builder.add(&key, vs, 0);
        }

        let tmp_dir = TempDir::new("agatedb").unwrap();
        let filename = tmp_dir.path().join("1.sst".to_string());

        let table = Table::create(&filename, builder.finish(), opts).unwrap();

        let mut it = table.new_iterator(0);
        assert!(it.valid());

        let mut count = 0;
        it.rewind();

        while it.valid() {
            assert_eq!(key(b"", count), user_key(it.key()));
            assert_eq!(value(count), it.value().value);
            it.next();
            count += 1;
        }

        assert!(!it.valid());
        assert_eq!(n, count);
        // TODO: support max_version in table
        // assert_eq!(n, table.max_version());
    }
}
