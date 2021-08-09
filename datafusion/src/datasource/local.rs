// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use crate::datasource::object_store::{ObjectReader, ObjectStore};
use crate::error::DataFusionError;
use crate::error::Result;
use crate::parquet::file::reader::Length;
use std::any::Any;
use std::fs;
use std::fs::{metadata, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::sync::Arc;

#[derive(Debug)]
pub struct LocalFileSystem;

impl ObjectStore for LocalFileSystem {
    fn as_any(&self) -> &dyn Any {
        return self;
    }

    fn list_all_files(&self, path: &str, ext: &str) -> Result<Vec<String>> {
        list_all(path, ext)
    }

    fn get_reader(&self, file_path: &str) -> Result<Arc<dyn ObjectReader>> {
        let file = File::open(file_path)?;
        let reader = LocalFSObjectReader::new(file)?;
        Ok(Arc::new(reader))
    }
}

struct LocalFSObjectReader {
    file: File,
}

impl LocalFSObjectReader {
    fn new(file: File) -> Result<Self> {
        Ok(Self { file })
    }
}

struct FileSegmentReader {
    reader: BufReader<File>,
    start: u64,
    length: usize,
}

impl FileSegmentReader {
    fn new(file: File, start: u64, length: usize) -> Self {
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Current(start as i64));
        Self {
            reader,
            start,
            length,
        }
    }
}

impl Read for FileSegmentReader {
    fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, std::io::Error> {
        self.reader.read(buf)
    }
}

impl ObjectReader for LocalFSObjectReader {
    fn get_reader(&self, start: u64, length: usize) -> Box<dyn Read> {
        Box::new(FileSegmentReader::new(
            self.file.try_clone().unwrap(),
            start,
            length,
        ))
    }

    fn length(&self) -> u64 {
        self.file.len()
    }
}

fn list_all(root_path: &str, ext: &str) -> Result<Vec<String>> {
    let mut filenames: Vec<String> = Vec::new();
    list_all_files(root_path, &mut filenames, ext);
    Ok(filenames)
}

/// Recursively build a list of files in a directory with a given extension with an accumulator list
fn list_all_files(dir: &str, filenames: &mut Vec<String>, ext: &str) -> Result<()> {
    let metadata = metadata(dir)?;
    if metadata.is_file() {
        if dir.ends_with(ext) {
            filenames.push(dir.to_string());
        }
    } else {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(path_name) = path.to_str() {
                if path.is_dir() {
                    list_all_files(path_name, filenames, ext)?;
                } else if path_name.ends_with(ext) {
                    filenames.push(path_name.to_string());
                }
            } else {
                return Err(DataFusionError::Plan("Invalid path".to_string()));
            }
        }
    }
    Ok(())
}
