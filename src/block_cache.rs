use random_access_storage::RandomAccess;
use std::io::Write;
use lru::LruCache;
use std::collections::HashMap;

#[derive(Debug,Clone)]
struct Block {
  pub data: Vec<u8>,
  pub mask: Vec<u8>,
  pub missing: usize
}

impl Block {
  pub fn new (size: usize) -> Self {
    let n = (size+7)/8;
    Self {
      data: vec![0;size],
      mask: vec![0;n],
      missing: size
    }
  }
  pub fn from_data (data: Vec<u8>) -> Self {
    let n = (data.len()+7)/8;
    Self {
      data,
      mask: vec![0;n],
      missing: 0
    }
  }
  pub fn write (&mut self, offset: usize, data: &[u8]) -> () {
    self.data[offset..offset+data.len()].copy_from_slice(data);
    for i in offset..offset+data.len() {
      let m = (self.mask[i/8] >> (i%8)) & 1 == 1;
      if !m && self.missing > 0 { self.missing -= 1 }
      self.mask[i/8] |= 1<<(i%8);
    }
  }
  pub fn merge (&mut self, data: &[u8]) -> () {
    for i in 0..data.len() {
      let m = (self.mask[i/8] >> (i%8)) & 1 == 1;
      if !m {
        self.data[i] = data[i];
        self.missing -= 1;
      }
    }
  }
  pub fn have_all (&self, i: usize, j: usize) -> bool {
    if self.missing == 0 { return true }
    for k in i..j {
      if (self.mask[k/8] >> (k%8)) & 1 == 0 { return false }
    }
    true
  }
  pub fn writes (&self) -> Vec<(usize,&[u8])> {
    if self.missing == 0 {
      vec![(0,self.data.as_slice())]
    } else {
      let mut result = vec![];
      let mut offset = 0;
      let mut prev = false;
      for i in 0..self.data.len() {
        let m = (self.mask[i/8] >> (i%8)) & 1 == 1;
        if m && !prev {
          offset = i;
        } else if !m && prev {
          result.push((offset,&self.data[offset..i]));
        }
        prev = m;
      }
      if prev && offset < self.data.len() {
        result.push((offset,&self.data[offset..]));
      }
      result
    }
  }
}

//#[derive(Debug,Clone)]
pub struct BlockCache<S> where S: RandomAccess {
  store: S,
  size: usize,
  reads: LruCache<u64,Block>,
  writes: HashMap<u64,Block>
}

impl<S> BlockCache<S> where S: RandomAccess {
  pub fn new (store: S, size: usize, count: usize) -> Self {
    Self {
      store,
      size,
      reads: LruCache::new(count),
      writes: HashMap::new()
    }
  }
}

impl<S> BlockCache<S> where S: RandomAccess {
  pub fn commit (&mut self) -> Result<(),S::Error> {
    let mut writes: Vec<(u64,Vec<u8>)> = vec![];
    for (b,block) in self.writes.iter() {
      self.reads.put(*b, block.clone());
      if writes.is_empty() {
        for (i,slice) in block.writes() {
          writes.push(((i as u64)+b,slice.to_vec()));
        }
      } else {
        for (i,slice) in block.writes() {
          if i == 0 {
            writes.last_mut().unwrap().1.extend_from_slice(slice);
          } else {
            writes.push(((i as u64)+b,slice.to_vec()));
          }
        }
      }
    }
    self.writes.clear();
    for (offset,data) in writes {
      self.store.write(offset as usize, &data)?;
    }
    Ok(())
  }
}

impl<S> RandomAccess for BlockCache<S> where S: RandomAccess {
  type Error = S::Error;
  fn write (&mut self, offset: usize, data: &[u8]) -> Result<(),Self::Error> {
    let start = (offset/self.size) as u64;
    let end = ((offset+data.len()+self.size-1)/self.size) as u64;
    let mut d_start = 0;
    for i in start..end {
      let b = i * (self.size as u64);
      let b_start = ((offset as u64).max(b)-b) as usize;
      let b_len = (((offset+data.len()) as u64 - b) as usize)
        .min(self.size - b_start)
        .min(data.len());
      let b_end = b_start + b_len;
      let d_end = d_start + b_len;
      let slice = &data[d_start..d_end];
      d_start += b_len;
      let check_read = match self.writes.get_mut(&b) {
        Some(block) => {
          block.write(b_start, slice);
          false
        },
        None => true
      };
      if check_read {
        match self.reads.pop(&b) {
          Some(mut block) => {
            block.data[b_start..b_end].copy_from_slice(slice);
            self.writes.insert(b, block);
          },
          None => {
            let mut block = Block::new(self.size);
            block.write(b_start, slice);
            self.writes.insert(b, block);
          }
        }
      }
    }
    Ok(())
  }
  fn read (&mut self, offset: usize, length: usize) ->
  Result<Vec<u8>,Self::Error> {
    let start = (offset/self.size) as u64;
    let end = ((offset+length+self.size-1)/self.size) as u64;
    let mut result: Vec<u8> = vec![0;length];
    let mut result_i = 0;
    let mut reads: Vec<(u64,(usize,usize),bool)> = vec![];
    for i in start..end {
      let b = i * (self.size as u64);
      let b_start = ((offset as u64).max(b)-b) as usize;
      let b_len = (((offset+length) as u64 - b) as usize)
        .min(self.size - b_start)
        .min(length);
      let b_end = b_start + b_len;
      let range = (result_i, result_i + b_len);
      result_i += b_len;
      match self.writes.get(&b) {
        Some(block) => {
          if block.have_all(b_start, b_end) {
            let slice = &block.data[b_start..b_end];
            result[range.0..range.1].copy_from_slice(slice);
          } else {
            reads.push((b,range,true));
          }
        },
        None => {
          match self.reads.get(&b) {
            Some(rblock) => {
              let slice = &rblock.data[b_start..b_end];
              result[range.0..range.1].copy_from_slice(slice);
            },
            None => { reads.push((b,range,false)) }
          }
        }
      };
    }
    if !reads.is_empty() {
      let len = self.store.len()? as u64;
      let i = reads[0].0.min(len);
      let j = (reads.last().unwrap().0 + (self.size as u64))
        .min(self.store.len()? as u64);
      let data = if j > i {
        self.store.read(i as usize, (j-i) as usize)?
      } else { vec![] };

      let len = data.len();
      for (b,range,write) in reads {
        let d_start = ((b-i) as usize).min(len);
        let d_end = (d_start + self.size).min(len);
        let slice = &data[d_start..d_end];

        let b_start = ((offset as u64).max(b)-b) as usize;
        let b_len = (((offset+length) as u64 - b) as usize)
          .min(self.size - b_start)
          .min(length);
        let b_end = b_start + b_len;

        if write {
          match self.writes.get_mut(&b) {
            Some(block) => {
              block.merge(&slice);
              let bslice = &block.data[b_start..b_end];
              result[range.0..range.1].copy_from_slice(bslice);
            },
            None => {
              panic!["expected block in write cache at offset {}", b]
            }
          }
        } else {
          let mut vdata = slice.to_vec();
          if slice.len() < self.size {
            vdata.extend(vec![0;self.size-slice.len()]);
          }
          let block = Block::from_data(vdata);
          {
            let bslice = &block.data[b_start..b_end];
            result[range.0..range.1].copy_from_slice(bslice);
          }
          self.reads.put(b, block);
        }
      }
    }
    assert_eq![result.len(), length, "correct result length"];
    Ok(result)
  }
  fn read_to_writer (&mut self, _offset: usize, _length: usize,
  _buf: &mut impl Write) -> Result<(),Self::Error> {
    unimplemented![]
  }
  fn del (&mut self, offset: usize, length: usize) -> Result<(),Self::Error> {
    self.store.del(offset, length)
  }
  fn truncate (&mut self, length: usize) -> Result<(),Self::Error> {
    self.store.truncate(length)
  }
  fn len (&mut self) -> Result<usize,Self::Error> {
    self.store.len()
  }
  fn is_empty (&mut self) -> Result<bool,Self::Error> {
    self.store.is_empty()
  }
}