// Copyright 2018 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The simplest cache. This should eventually offer line-oriented access
//! to the remote document, and can be used as a building block for more
//! complicated caching schemes.

use memchr::memchr;

use xi_rope::rope::{Rope, RopeDelta, LinesMetric};
use xi_rope::delta::DeltaElement;
use xi_core::plugin_rpc::{TextUnit, GetDataResponse};

use plugin_base::{Error, DataSource};

const CHUNK_SIZE: usize = 1024 * 1024;

/// A simple cache, holding a single contiguous chunk of the document.
#[derive(Debug, Clone, Default)]
pub struct ChunkCache {
    /// The position of this chunk relative to the tracked document.
    /// All offsets are guaranteed to be valid UTF-8 character boundaries.
    pub offset: usize,
    /// A chunk of the remote buffer.
    pub contents: String,
    /// The (zero-based) line number of the line containing the start of the chunk.
    pub first_line: usize,
    /// The byte offset of the start of the chunk from the start of `first_line`.
    /// If this chunk starts at a line break, this will be 0.
    pub first_line_offset: usize,
    /// A list of indexes of newlines in this chunk.
    pub line_offsets: Vec<usize>,
    /// The total size of the tracked document.
    pub buf_size: usize,
    pub num_lines: usize,
    pub rev: u64,
}

impl ChunkCache {
    /// Returns the text of the line at `line_num`, zero-indexed, fetching
    /// data from `source` if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if `line_num` is greater than the total number of lines
    /// in the document, or if there is a problem communicating with `source`.
    pub fn get_line<DS>(&mut self, source: &DS, line_num: usize) -> Result<&str, Error>
        where DS: DataSource
    {
        if line_num > self.num_lines { return Err(Error::BadRequest) }

        // if chunk does not include the start of this line, fetch and reset everything
        if self.contents.len() == 0
            || line_num < self.first_line
            || (line_num == self.first_line && self.first_line_offset > 0) {
                let resp = source.get_data(line_num, TextUnit::Line, CHUNK_SIZE, self.rev)?;
                self.reset_chunk(resp);
        }

        // We now know that the start of this line is contained in self.contents.
        let mut start_off = self.cached_offset_of_line(line_num).unwrap();

        // Now we make sure we also contain the end of the line, fetching more
        // of the document as necessary.
        loop {
            if let Some(end_off) = self.cached_offset_of_line(line_num + 1) {
                return Ok(&self.contents[start_off..end_off])
            }
            // if we have a chunk and we're fetching more, discard unnecessary
            // portion of our chunk.
            if start_off != 0 {
                self.clear_up_to(start_off);
                start_off = 0;
            }

            let chunk_end = self.offset + self.contents.len();
            let resp = source.get_data(chunk_end, TextUnit::Utf8,
                                       CHUNK_SIZE, self.rev)?;
            self.append_chunk(resp);
        }
    }

    /// Returns the offset of the line at `line_num`, zero-indexed, fetching
    /// data from `source` if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if `line_num` is greater than the total number of lines
    /// in the document, or if there is a problem communicating with `source`.
    pub fn offset_of_line<DS>(&mut self, source: &DS, line_num: usize) -> Result<usize, Error>
        where DS: DataSource
    {
        if line_num > self.num_lines { return Err(Error::BadRequest) }
        match self.cached_offset_of_line(line_num) {
            Some(offset) => Ok(offset + self.offset),
            None => {
                let resp = source.get_data(line_num, TextUnit::Line, CHUNK_SIZE, self.rev)?;
                self.reset_chunk(resp);
                self.offset_of_line(source, line_num)
            }
        }
    }

    /// Returns the offset of the provided `line_num` in `self.contents` if
    /// it is present in the chunk.
    fn cached_offset_of_line(&self, line_num: usize) -> Option<usize> {
        if line_num < self.first_line { return None }

        let rel_line_num = line_num - self.first_line;

        if rel_line_num == 0 && self.first_line_offset == 0 {
            return Some(0)
        }
        if rel_line_num <= self.line_offsets.len() {
            return Some(self.line_offsets[rel_line_num - 1])
        }

        // EOF
        if line_num == self.num_lines && self.offset + self.contents.len() == self.buf_size {
            return Some(self.contents.len())
        }
        None
    }

    /// Clears anything in the cache up to `offset`, which is indexed relative
    /// to `self.contents`.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is not a character boundary, or if `offset` is greater than
    /// the length of `self.content`.
    fn clear_up_to(&mut self, offset: usize) {
        if offset > self.contents.len() {
            panic!("offset greater than content length: {} > {}", offset, self.contents.len())
        }

        let new_contents = self.contents.split_off(offset);
        self.contents = new_contents;
        self.offset += offset;
        // first find out if offset is a line offset, and set first_line / first_line_offset
        let (new_line, new_line_off) = match self.line_offsets.binary_search(&offset) {
            Ok(idx) => (self.first_line + idx + 1, 0),
            Err(0) => (self.first_line, self.first_line_offset + offset),
            Err(idx) => (self.first_line + idx, offset - self.line_offsets[idx - 1]),
        };

        // then clear line_offsets up to and including offset
        self.line_offsets = self.line_offsets.iter()
            .filter(|i| **i > offset)
            .map(|i| i - offset)
            .collect();

        self.first_line = new_line;
        self.first_line_offset = new_line_off;
    }

    /// Discard any existing cache, starting again with the new data.
    fn reset_chunk(&mut self, data: GetDataResponse) {
        self.contents = data.chunk;
        self.offset = data.offset;
        self.first_line = data.first_line;
        self.first_line_offset = data.first_line_offset;
        self.recalculate_line_offsets();
    }

    /// Append to the existing cache, leaving existing data in place.
    fn append_chunk(&mut self, data: GetDataResponse) {
        self.contents.push_str(data.chunk.as_str());
        // this is doing extra work in the case where we're fetching a single
        // massive (multiple of CHUNK_SIZE) line, but unclear if it's worth optimizing
        self.recalculate_line_offsets();
    }

    fn recalculate_line_offsets(&mut self) {
        self.line_offsets.clear();
        newline_offsets(&self.contents, &mut self.line_offsets);
    }

    /// Updates the chunk to reflect changes in this delta.
    pub fn apply_update(&mut self, new_len: usize, num_lines: usize,
                        rev: u64, delta: Option<&RopeDelta>) {
        let is_empty = self.offset == 0 && self.contents.len() == 0;
        let should_clear = match delta {
            Some(delta) if !is_empty => self.should_clear(delta),
            // if no contents, clearing is a noop
            Some(_) => true,
            // no delta means a very large edit
            None => true,
        };

        if should_clear {
            self.clear();
        } else {
            // only reached if delta exists
            self.update_chunk(delta.unwrap());
        }
        self.buf_size = new_len;
        self.num_lines =  num_lines;
        self.rev = rev;
    }

    /// Determine whether we should update our state with this delta,
    /// or if we should clear it. In the update case, also patches up
    /// offsets.
    fn should_clear(&mut self, delta: &RopeDelta) -> bool {
        let (iv, _) = delta.summary();
        let start = iv.start();
        let end = iv.end();
        // we only apply the delta if it is a simple edit, which
        // begins in the interior of our chunk.
        // - If it begins _before_ our chunk, we are likely going to
        // want to fetch the edited region, which will reset our state;
        // - If it begins _after_ our chunk, it has no effect on our state;
        // - If it's a complex edit the logic is tricky, and this should
        // be rare enough we can afford to discard.
        // The one 'complex edit' we should probably be handling is
        // the replacement of a single range. This could be a new
        // convenience method on `Delta`?
        if start < self.offset || start >= self.offset + self.contents.len() {
            true
        } else if delta.is_simple_delete() {
            self.simple_delete(start, end);
            false
        } else if let Some(text) = delta.as_simple_insert() {
            assert_eq!(iv.size(), 0);
            self.simple_insert(text, start);
            false
        } else {
            true
        }
    }

    /// Patches up `self.line_offsets` in the simple insert case.
    fn simple_insert(&mut self, text: &Rope, ins_offset: usize) {
        let has_newline = text.measure::<LinesMetric>() > 0;
        let self_off = self.offset;
        assert!(ins_offset >= self_off);
        //let ins_offset = ins_offset + self.offset;
        // regardless of if we are inserting newlines we adjust offsets
        self.line_offsets.iter_mut()
            .for_each(|off| if *off > ins_offset - self_off { *off += text.len() });
        // calculate and insert new newlines if necessary
        // we could save some hassle and just rerun memchr on the chunk here?
        if has_newline {
            let mut new_offsets = Vec::new();
            newline_offsets(&String::from(text), &mut new_offsets);
            new_offsets.iter_mut().for_each(|off| *off += ins_offset - self_off);

            let split_idx = self.line_offsets.binary_search(&new_offsets[0])
                .err()
                .expect("new index cannot be occupied");

            self.line_offsets = [
                &self.line_offsets[..split_idx],
                &new_offsets,
                &self.line_offsets[split_idx..],
            ].concat();
        }
    }

    /// Patches up `self.line_offsets` in the simple delete case.
    fn simple_delete(&mut self, start: usize, end: usize) {
        let del_size = end - start;
        let start = start - self.offset;
        let end = end - self.offset;
        let has_newline = memchr(b'\n', &self.contents.as_bytes()[start..end])
            .is_some();
        // a bit too fancy: only reallocate if we need to remove an item
        if has_newline {
            self.line_offsets = self.line_offsets.iter()
                .filter_map(|off| match *off {
                    x if x < start => Some(x),
                    x if x >= start && x < end => None,
                    x if x >= end => Some(x - del_size),
                    hmm => panic!("invariant violated {} {} {}?", start, end, hmm),
                })
            .collect();
        } else {
            self.line_offsets.iter_mut()
                .for_each(|off| if *off >= end { *off -= del_size });
        }
    }

    /// Updates `self.contents` with the given delta.
    fn update_chunk(&mut self, delta: &RopeDelta) {
        let chunk_start = self.offset;
        let chunk_end = chunk_start + self.contents.len();
        let mut new_state = String::with_capacity(self.contents.len());
        let mut prev_copy_end = 0;
        let mut del_before: usize = 0;
        let mut ins_before: usize = 0;

        for op in delta.els.as_slice() {
            match op {
                &DeltaElement::Copy(start, end) => {
                    if start < chunk_start {
                        del_before += start - prev_copy_end;
                        if end >= chunk_start {
                            let cp_end = (end - chunk_start).min(self.contents.len());
                            new_state.push_str(&self.contents[0..cp_end]);
                        }
                    } else if start <= chunk_end {
                        if prev_copy_end < chunk_start {
                            del_before += chunk_start - prev_copy_end;
                        }
                        let cp_start = start - chunk_start;
                        let cp_end = (end - chunk_start).min(self.contents.len());
                        new_state.push_str(&self.contents[cp_start .. cp_end]);
                    }
                    prev_copy_end = end;
                }
                &DeltaElement::Insert(ref s) => {
                    if prev_copy_end < chunk_start {
                        ins_before += s.len();
                    } else if prev_copy_end <= chunk_end {
                        let s: String = s.into();
                        new_state.push_str(&s);
                    }
                }
            }
        }
        self.offset += ins_before;
        self.offset -= del_before;
        self.contents = new_state;
    }

    pub fn clear(&mut self) {
        self.contents.clear();
        self.offset = 0;
        self.line_offsets.clear();
        self.first_line = 0;
        self.first_line_offset = 0;
    }
}

/// Calculates the offsets of newlines in `text`,
/// inserting the results into `storage`. The offsets are the offset
/// of the start of the line, not the line break character.
fn newline_offsets(text: &str, storage: &mut Vec<usize>) {
    let mut cur_idx = 0;
    while let Some(idx) = memchr(b'\n', &text.as_bytes()[cur_idx..]) {
        storage.push(cur_idx + idx + 1);
        cur_idx += idx + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xi_rope::interval::Interval;
    use xi_rope::delta::Delta;
    use xi_rope::rope::{Rope, LinesMetric};
    use xi_core::plugin_rpc::GetDataResponse;

    struct MockDataSource(Rope);

    impl DataSource for MockDataSource {
        fn get_data(&self, start: usize, unit: TextUnit, _max_size: usize, _rev: u64)
            -> Result<GetDataResponse, Error> {
            let offset = unit.resolve_offset(&self.0, start)
                .ok_or(Error::Other("unable to resolve offset".into()))?;
            let first_line = self.0.line_of_offset(offset);
            let first_line_offset = offset - self.0.offset_of_line(first_line);
            let end_off = (offset + CHUNK_SIZE).min(self.0.len());

            // not the right error, but okay for this
            if offset > self.0.len() {
                Err(Error::Other("offset too big".into()))
            } else {
                let chunk = self.0.slice_to_string(offset, end_off);
                Ok(GetDataResponse { chunk, offset, first_line, first_line_offset })
            }
        }
    }

    #[test]
    fn simple_chunk() {
        let mut c = ChunkCache::default();
        c.buf_size = 2;
        c.contents = "oh".into();

        let d = Delta::simple_edit(Interval::new_closed_open(0, 0), "yay".into(), c.contents.len());
        c.apply_update(d.new_document_len(), 1, 1, Some(&d));
        assert_eq!(&c.contents, "yayoh");
        assert_eq!(c.offset, 0);

        let d = Delta::simple_edit(Interval::new_closed_open(0, 0), "ahh".into(), c.contents.len());
        c.apply_update(d.new_document_len(), 1, 2, Some(&d));

        assert_eq!(&c.contents, "ahhyayoh");
        assert_eq!(c.offset, 0);

        let d = Delta::simple_edit(Interval::new_closed_open(2, 2), "_oops_".into(), c.contents.len());
        assert_eq!(d.els.len(), 3);
        c.apply_update(d.new_document_len(), 1, 3, Some(&d));

        assert_eq!(&c.contents, "ah_oops_hyayoh");
        assert_eq!(c.offset, 0);

        let d = Delta::simple_edit(Interval::new_closed_open(9, 9), "fin".into(), c.contents.len());
        c.apply_update(d.new_document_len(), 1, 5, Some(&d));

        assert_eq!(&c.contents, "ah_oops_hfinyayoh");
        assert_eq!(c.offset, 0);
    }

    #[test]
    fn get_lines() {
        let remote_document = MockDataSource("this\nhas\nfour\nlines!".into());
        let mut c = ChunkCache::default();
        c.buf_size = remote_document.0.len();
        c.num_lines = remote_document.0.measure::<LinesMetric>() + 1;
        assert_eq!(c.num_lines, 4);
        assert_eq!(c.buf_size, 20);
        assert_eq!(c.line_offsets.len(), 0);
        assert_eq!(c.get_line(&remote_document, 0).ok(), Some("this\n"));
        assert_eq!(c.line_offsets.len(), 3);
        assert_eq!(c.offset, 0);
        assert_eq!(c.buf_size, 20);
        assert_eq!(c.contents.len(), 20);
        assert_eq!(c.get_line(&remote_document, 2).ok(), Some("four\n"));
        assert_eq!(c.cached_offset_of_line(4), Some(20));
        assert_eq!(c.get_line(&remote_document, 3).ok(), Some("lines!"));
        assert!(c.get_line(&remote_document, 4).is_err());
    }

    #[test]
    fn reset_chunk() {
        let data = GetDataResponse {
            chunk: "1\n2\n3\n4\n5\n6\n7".into(),
            offset: 0,
            first_line: 0,
            first_line_offset: 0,
        };
        let mut cache = ChunkCache::default();
        cache.reset_chunk(data);

        assert_eq!(cache.line_offsets.len(), 6);
        assert_eq!(cache.line_offsets, vec![2, 4, 6, 8, 10, 12]);

        let idx_1 = cache.cached_offset_of_line(1).unwrap();
        let idx_2 = cache.cached_offset_of_line(2).unwrap();
        assert_eq!(&cache.contents.as_str()[idx_1..idx_2], "2\n");
    }

    #[test]
    fn clear_up_to() {
        let mut c = ChunkCache::default();
        let data = GetDataResponse {
            chunk: "this\n has a newline at idx 4\nand at idx 28".into(),
            offset: 0,
            first_line: 0,
            first_line_offset: 0,
        };
        c.reset_chunk(data);
        assert_eq!(c.line_offsets, vec![5, 29]);
        c.clear_up_to(5);
        assert_eq!(c.offset, 5);
        assert_eq!(c.first_line, 1);
        assert_eq!(c.first_line_offset, 0);
        assert_eq!(c.line_offsets, vec![24]);

        c.clear_up_to(10);
        assert_eq!(c.offset, 15);
        assert_eq!(c.first_line, 1);
        assert_eq!(c.first_line_offset, 10);
        assert_eq!(c.line_offsets, vec![14]);
    }

    #[test]
    fn simple_insert() {
        let mut c = ChunkCache::default();
        c.contents = "some".into();
        c.buf_size = 4;
        let d = Delta::simple_edit(Interval::new_closed_open(0, 0),
                                   "two\nline\nbreaks".into(), c.contents.len());
        assert!(d.as_simple_insert().is_some());
        assert!(!d.is_simple_delete());
        c.apply_update(d.new_document_len(), 3, 1, Some(&d));
        assert_eq!(c.line_offsets, vec![4, 9]);

        let d = Delta::simple_edit(Interval::new_closed_open(4, 4),
                                   "one\nmore".into(), c.contents.len());
        assert!(d.as_simple_insert().is_some());
        c.apply_update(d.new_document_len(), 4, 2, Some(&d));
        assert_eq!(&c.contents, "two\none\nmoreline\nbreakssome");
        assert_eq!(c.line_offsets, vec![4, 8, 17]);
    }
    #[test]
    fn offset_of_line() {
        let source = MockDataSource("this\nhas\nfour\nlines!".into());
        let mut c = ChunkCache::default();
        c.buf_size = source.0.len();
        c.num_lines = source.0.measure::<LinesMetric>() + 1;
        assert_eq!(c.num_lines, 4);
        assert_eq!(c.cached_offset_of_line(0), Some(0));
        assert_eq!(c.offset_of_line(&source, 0).unwrap(), 0);
        assert_eq!(c.offset_of_line(&source, 1).unwrap(), 5);
        assert_eq!(c.offset_of_line(&source, 2).unwrap(), 9);
        assert_eq!(c.offset_of_line(&source, 3).unwrap(), 14);
    }

    #[test]
    fn simple_edits_with_offset() {
        let mut source = MockDataSource("this\nhas\nfour\nlines!".into());
        let mut c = ChunkCache::default();
        c.buf_size = source.0.len();
        c.num_lines = source.0.measure::<LinesMetric>() + 1;
        // get line fetches from source, starting at this line
        assert_eq!(c.get_line(&source, 2).ok(), Some("four\n"));
        assert_eq!(c.offset, 9);
        assert_eq!(&c.contents, "four\nlines!");
        assert_eq!(c.offset_of_line(&source, 3).unwrap(), 14);
        let d = Delta::simple_edit(Interval::new_closed_open(10,10),
                                   "ive nice\ns".into(), c.contents.len() + c.offset);
        c.apply_update(d.new_document_len(), 5, 1, Some(&d));
        // keep our source up to date
        source.0 = "this\nhas\nfive nice\nsour\nlines!".into();

        assert_eq!(&c.contents, "five nice\nsour\nlines!");
        assert_eq!(c.offset, 9);
        assert_eq!(c.offset_of_line(&source, 3).unwrap(), 19);
        assert_eq!(c.offset_of_line(&source, 4).unwrap(), 24);
        // this isn't in the chunk, so should cause a fetch that brings in the whole buffer
        assert_eq!(c.offset_of_line(&source, 0).unwrap(), 0);
        assert_eq!(c.offset, 0);
        assert_eq!(&c.contents, "this\nhas\nfive nice\nsour\nlines!");
        assert_eq!(c.offset_of_line(&source, 1).unwrap(), 5);
        assert_eq!(c.offset_of_line(&source, 3).unwrap(), 19);
        assert_eq!(c.offset_of_line(&source, 4).unwrap(), 24);

        // reset and fetch the middle, so we have an offset:
        c.clear_up_to(5);
        assert_eq!(&c.contents, "has\nfive nice\nsour\nlines!");
        assert_eq!(c.offset, 5);
        assert_eq!(c.first_line, 1);
        assert_eq!(c.offset_of_line(&source, 2).unwrap(), 9);
        let d = Delta::simple_edit(Interval::new_closed_open(6, 10),
                                   "".into(), c.contents.len() + c.offset);
        assert!(d.is_simple_delete());
        c.apply_update(d.new_document_len(), 4, 1, Some(&d));

        assert_eq!(&c.contents, "hive nice\nsour\nlines!");
        assert_eq!(c.offset, 5);
        assert_eq!(c.first_line, 1);
        assert_eq!(c.offset_of_line(&source, 2).unwrap(), 15);
    }
}
