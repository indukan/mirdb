use crate::block::Block;
use crate::block_handle::BlockHandle;
use crate::block_iter::BlockIter;
use crate::block_iter::BlockIterState;
use crate::types::SsIterator;
use crate::TableReader;

pub struct TableIter<'a> {
    table: &'a TableReader,
    pub(crate) index_iter: BlockIter<'a>,
    data_iter_state: BlockIterState,
    data_block: Option<Block>,
}

impl<'a> TableIter<'a> {
    pub fn new(table: &'a TableReader) -> Self {
        Self {
            table,
            index_iter: table.index_block.iter(),
            data_iter_state: BlockIterState::new(0),
            data_block: None,
        }
    }

    fn data_iter(&self) -> Option<BlockIter> {
        match &self.data_block {
            Some(ref v) => Some(BlockIter::new_with_state(
                &v.block,
                self.data_iter_state.clone(),
            )),
            _ => None,
        }
    }

    #[cfg(test)]
    fn print_info(&self) {
        println!("------------------------------------");
        println!("data_block: {}", self.data_block.is_some());
        println!("index_iter: {:?}", self.index_iter.state);
        println!("data_iter : {:?}", self.data_iter_state);
        println!("------------------------------------");
    }
}

impl<'a> SsIterator for TableIter<'a> {
    fn valid(&self) -> bool {
        let data_iter = self.data_iter();
        data_iter.is_some() && data_iter.as_ref().unwrap().valid()
    }

    fn advance(&mut self) -> bool {
        if let Some(data_iter) = &mut self.data_iter() {
            if data_iter.advance() {
                self.data_iter_state = data_iter.state.clone();
                return true;
            }
            self.data_iter_state = data_iter.state.clone();
        }

        if !self.index_iter.advance() {
            return false;
        }

        if let Some((_k, v)) = self.index_iter.current_kv() {
            let (bh, _) = BlockHandle::decode(&v);
            match self.table.read_block(&bh) {
                Ok(Some(block)) => {
                    self.data_iter_state = BlockIterState::new(block.restarts_offset());
                    self.data_block = Some(block);
                    return self.advance();
                }
                Ok(None) => {
                    return false;
                }
                Err(_) => return self.advance(),
            }
        }

        false
    }

    fn prev(&mut self) -> bool {
        if let Some(data_iter) = &mut self.data_iter() {
            if data_iter.prev() {
                self.data_iter_state = data_iter.state.clone();
                return true;
            }

            self.data_iter_state = data_iter.state.clone();
        }

        if !self.index_iter.prev() {
            self.reset();
            return false;
        }

        if let Some((_k, v)) = self.index_iter.current_kv() {
            let (bh, _) = BlockHandle::decode(&v);
            if let Ok(Some(block)) = self.table.read_block(&bh) {
                let mut iter = block.iter();
                iter.seek_to_last();
                self.data_iter_state = iter.state;
                self.data_block = Some(block);
                return true;
            }
        }

        self.reset();
        false
    }

    fn current_k(&self) -> Option<Vec<u8>> {
        self.data_iter().and_then(|x| x.current_k())
    }

    fn current_v(&self) -> Option<Vec<u8>> {
        self.data_iter().and_then(|x| x.current_v())
    }

    fn reset(&mut self) {
        self.index_iter.reset();
        self.data_block = None;
        self.data_iter_state.reset();
    }

    fn seek(&mut self, key: &[u8]) {
        self.reset();
        self.index_iter.seek(key);
        if let Some((_k, v)) = self.index_iter.current_kv() {
            let (bh, _) = BlockHandle::decode(&v);
            if let Ok(Some(block)) = self.table.read_block(&bh) {
                let mut iter = block.iter();
                iter.seek(key);
                self.data_iter_state = iter.state;
                self.data_block = Some(block);
            }
        }
    }

    fn seek_to_last(&mut self) {
        self.reset();
        self.index_iter.seek_to_last();
        self.index_iter.prev();
        self.advance();

        assert!(self.valid());
    }
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use crate::table_builder::TableBuilder;
    use crate::util::to_str;
    use crate::MyResult;
    use crate::Options;

    use super::*;

    static N: usize = 100;

    fn get_data() -> Vec<(String, String)> {
        let mut data = vec![];
        let mut key_prefix = "prefix_key".to_owned();
        let value_prefix = "value";
        for i in 1..=N {
            if i % 10 == 0 {
                key_prefix += "a";
            }
            let key = format!("{}{}", key_prefix, i);
            let value = format!("{}{}", value_prefix, i);
            data.push((key, value));
        }
        data
    }

    #[test]
    fn test_seek() -> MyResult<()> {
        let path = Path::new("/tmp/test_table_iter");
        let mut opt = Options::default();
        opt.block_size = 1;
        let mut t = TableBuilder::new(path, opt.clone())?;
        let data = get_data();
        for (k, v) in &data {
            t.add(k.as_bytes(), v.as_bytes())?;
        }
        t.flush()?;

        let t = TableReader::new(path, opt.clone())?;

        let mut iter = TableIter::new(&t);
        assert_eq!(None, iter.current_kv());
        for i in 0..N {
            let key = &data[i].0;
            iter.seek(key.as_bytes());
            if iter.current_k().unwrap() != key.as_bytes() {
                println!(
                    "error seek i: {} k: {}",
                    i,
                    to_str(&iter.current_k().unwrap().to_vec())
                );
                println!("v: {}", to_str(&iter.current_v().unwrap()));
                assert!(false);
            }
        }
        Ok(())
    }

    #[test]
    fn test_advance_prev() -> MyResult<()> {
        let path = Path::new("/tmp/test_table_advance_prev");
        let mut opt = Options::default();
        opt.block_size = 20;
        let mut t = TableBuilder::new(path, opt.clone())?;
        let data = get_data();
        for (k, v) in &data {
            t.add(k.as_bytes(), v.as_bytes())?;
        }
        t.flush()?;
        let t = TableReader::new(path, opt.clone())?;

        let mut iter = TableIter::new(&t);
        println!("advance before");
        iter.print_info();
        assert!(iter.advance());
        println!("advance after");
        iter.print_info();
        assert_eq!(Some(b"prefix_key1".to_vec()), iter.current_k());
        assert!(iter.advance());
        assert_eq!(Some(b"prefix_key2".to_vec()), iter.current_k());
        assert!(iter.prev());
        assert_eq!(Some(b"prefix_key1".to_vec()), iter.current_k());
        assert!(iter.advance());
        assert_eq!(Some(b"prefix_key2".to_vec()), iter.current_k());
        assert!(iter.advance());
        assert_eq!(Some(b"prefix_key3".to_vec()), iter.current_k());
        assert!(iter.prev());
        assert_eq!(Some(b"prefix_key2".to_vec()), iter.current_k());
        assert!(iter.prev());
        assert_eq!(Some(b"prefix_key1".to_vec()), iter.current_k());
        println!("prev before");
        iter.print_info();
        assert!(!iter.prev());
        println!("prev after");
        iter.print_info();
        assert_eq!(None, iter.current_k());
        println!("advance before");
        iter.print_info();
        assert!(iter.advance());
        println!("advance after");
        iter.print_info();
        assert_eq!(Some(b"prefix_key1".to_vec()), iter.current_k());
        println!("advance before");
        iter.print_info();
        assert!(iter.advance());
        println!("advance after");
        iter.print_info();
        assert_eq!(Some(b"prefix_key2".to_vec()), iter.current_k());
        assert!(iter.prev());
        assert_eq!(Some(b"prefix_key1".to_vec()), iter.current_k());
        assert!(!iter.prev());
        assert_eq!(None, iter.current_k());

        for i in 0..N {
            assert!(iter.advance());
            let key = &data[i].0;
            if iter.current_k().unwrap() != key.as_bytes() {
                println!(
                    "error advance i: {} k: {} current_k: {}",
                    i,
                    to_str(key.as_bytes()),
                    to_str(&iter.current_k().unwrap().to_vec())
                );
                assert!(false);
            }
        }
        println!("advance before");
        iter.print_info();
        assert!(!iter.advance());
        println!("advance after");
        iter.print_info();
        assert_eq!(None, iter.current_kv());
        assert!(!iter.advance());
        assert_eq!(None, iter.current_kv());

        for i in (0..N - 1).into_iter().rev() {
            iter.prev();
            let key = &data[i].0;
            if iter.current_k().unwrap() != key.as_bytes() {
                println!(
                    "error prev i: {} k: {}",
                    i,
                    to_str(&iter.current_k().unwrap().to_vec())
                );
                assert!(false);
            }
        }

        assert!(!iter.prev());
        assert_eq!(None, iter.current_kv());
        assert!(!iter.prev());
        assert_eq!(None, iter.current_kv());

        let mut iter = TableIter::new(&t);
        iter.seek_to_last();
        assert_eq!(iter.current_k().unwrap(), data[N - 1].0.as_bytes());
        for i in (0..N - 1).into_iter().rev() {
            iter.prev();
            let key = &data[i].0;
            if iter.current_k().unwrap() != key.as_bytes() {
                println!(
                    "error prev i: {} k: {}",
                    i,
                    to_str(&iter.current_k().unwrap().to_vec())
                );
                assert!(false);
            }
        }
        iter.prev();
        assert_eq!(None, iter.current_kv());
        Ok(())
    }
}
