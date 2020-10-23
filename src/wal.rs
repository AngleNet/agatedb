use crate::Result;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use crate::structs::Entry;

struct Header {
    key_len: usize,
    value_len: usize,
}

pub struct Wal {
    file_id: usize,
    path: PathBuf,
}

impl Wal {
    pub fn open(file_id: usize, path: PathBuf) -> Result<Wal> {
        let f = OpenOptions::new().append(true).create(true).open(&path)?;
        Ok(Wal { file_id, path })
    }

    pub fn write_entry(&mut self, entry: Entry) -> Result<()> {
        unimplemented!()
    }
}
