//! Syncronous IO
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Range,
    result,
};

use blake3::guts::parent_cv;
use bytes::BytesMut;
use range_collections::{range_set::RangeSetRange, RangeSet2, RangeSetRef};
use smallvec::SmallVec;

use crate::{
    hash_block, hash_chunk,
    io::error::{DecodeError, EncodeError},
    iter::{BaoChunk, PreOrderChunkIterRef},
    range_ok, BaoTree, BlockSize, ByteNum, ChunkNum, TreeNode,
};

/// An outboard is a just a thing that knows how big it is and can get you the hashes for a node.
pub trait Outboard {
    /// The root hash
    fn root(&self) -> blake3::Hash;
    /// The tree. This contains the information about the size of the file and the block size.
    fn tree(&self) -> BaoTree;
    /// load the hash pair for a node
    fn load(&self, node: TreeNode) -> io::Result<Option<(blake3::Hash, blake3::Hash)>>;
}

pub trait OutboardMut: Outboard {
    /// Set the length of the file for which this outboard is
    fn set_size(&mut self, len: ByteNum) -> io::Result<()>;
    /// Save a hash pair for a node
    fn save(&mut self, node: TreeNode, hash_pair: &(blake3::Hash, blake3::Hash)) -> io::Result<()>;
}

impl<O: Outboard> Outboard for &O {
    fn root(&self) -> blake3::Hash {
        (**self).root()
    }
    fn tree(&self) -> BaoTree {
        (**self).tree()
    }
    fn load(&self, node: TreeNode) -> io::Result<Option<(blake3::Hash, blake3::Hash)>> {
        (**self).load(node)
    }
}

impl<O: Outboard> Outboard for &mut O {
    fn root(&self) -> blake3::Hash {
        (**self).root()
    }
    fn tree(&self) -> BaoTree {
        (**self).tree()
    }
    fn load(&self, node: TreeNode) -> io::Result<Option<(blake3::Hash, blake3::Hash)>> {
        (**self).load(node)
    }
}

impl<O: OutboardMut> OutboardMut for &mut O {
    fn save(&mut self, node: TreeNode, hash_pair: &(blake3::Hash, blake3::Hash)) -> io::Result<()> {
        (**self).save(node, hash_pair)
    }
    fn set_size(&mut self, len: ByteNum) -> io::Result<()> {
        (**self).set_size(len)
    }
}

/// Given an outboard, return a range set of all valid ranges
pub fn valid_ranges<O>(outboard: &O) -> io::Result<RangeSet2<ChunkNum>>
where
    O: Outboard,
{
    struct RecursiveValidator<'a, O: Outboard> {
        tree: BaoTree,
        valid_nodes: TreeNode,
        res: RangeSet2<ChunkNum>,
        outboard: &'a O,
    }

    impl<'a, O: Outboard> RecursiveValidator<'a, O> {
        fn validate_rec(
            &mut self,
            parent_hash: &blake3::Hash,
            node: TreeNode,
            is_root: bool,
        ) -> io::Result<()> {
            let (l_hash, r_hash) = if let Some((l_hash, r_hash)) = self.outboard.load(node)? {
                let actual = parent_cv(&l_hash, &r_hash, is_root);
                if &actual != parent_hash {
                    // we got a validation error. Simply continue without adding the range
                    return Ok(());
                }
                (l_hash, r_hash)
            } else {
                (*parent_hash, blake3::Hash::from([0; 32]))
            };
            if let Some(leaf) = node.as_leaf() {
                let start = self.tree.chunk_num(leaf);
                let end = (start + self.tree.chunk_group_chunks() * 2).min(self.tree.chunks());
                self.res |= RangeSet2::from(start..end);
            } else {
                // recurse
                let left = node.left_child().unwrap();
                self.validate_rec(&l_hash, left, false)?;
                let right = node.right_descendant(self.valid_nodes).unwrap();
                self.validate_rec(&r_hash, right, false)?;
            }
            Ok(())
        }
    }
    let tree = outboard.tree();
    let root_hash = outboard.root();
    let mut validator = RecursiveValidator {
        tree,
        valid_nodes: tree.filled_size(),
        res: RangeSet2::empty(),
        outboard,
    };
    validator.validate_rec(&root_hash, tree.root(), true)?;
    Ok(validator.res)
}

/// A reader that can read a slice at a specified offset
///
/// For a file, this will be implemented by seeking to the offset and then reading the data.
/// For other types of storage, seeking is not necessary. E.g. a Bytes or a memory mapped
/// slice already allows random access.
///
/// This is similar to the io interface of sqlite.
/// See xRead, xFileSize in <https://www.sqlite.org/c3ref/io_methods.html>
#[allow(clippy::len_without_is_empty)]
pub trait SliceReader {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn len(&mut self) -> io::Result<u64>;
}

impl<R: Read + Seek> SliceReader for R {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.seek(SeekFrom::Start(offset))?;
        self.read_exact(buf)
    }

    fn len(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::End(0))
    }
}

/// A writer that can write a slice at a specified offset
///
/// Will extend the file if the offset is past the end of the file, just like posix
/// and windows files do.
///
/// This is similar to the io interface of sqlite.
/// See xWrite in <https://www.sqlite.org/c3ref/io_methods.html>
pub trait SliceWriter {
    fn write_at(&mut self, offset: u64, src: &[u8]) -> io::Result<()>;
}

impl<W: Write + Seek> SliceWriter for W {
    fn write_at(&mut self, offset: u64, src: &[u8]) -> io::Result<()> {
        self.seek(SeekFrom::Start(offset))?;
        self.write_all(src)
    }
}

use super::{DecodeResponseItem, Header, Leaf, Parent};

// When this enum is used it is in the Header variant for the first 8 bytes, then stays in
// the Content state for the remainder.  Since the Content is the largest part that this
// size inbalance is fine, hence allow clippy::large_enum_variant.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum Position<'a> {
    /// currently reading the header, so don't know how big the tree is
    /// so we need to store the ranges and the chunk group log
    Header {
        ranges: &'a RangeSetRef<ChunkNum>,
        block_size: BlockSize,
    },
    /// currently reading the tree, all the info we need is in the iter
    Content { iter: PreOrderChunkIterRef<'a> },
}

#[derive(Debug)]
pub struct DecodeResponseIter<'a, R> {
    inner: Position<'a>,
    stack: SmallVec<[blake3::Hash; 10]>,
    encoded: R,
    buf: BytesMut,
}

impl<'a, R: Read> DecodeResponseIter<'a, R> {
    pub fn new(
        root: blake3::Hash,
        block_size: BlockSize,
        encoded: R,
        ranges: &'a RangeSetRef<ChunkNum>,
        buf: BytesMut,
    ) -> Self {
        let mut stack = SmallVec::new();
        stack.push(root);
        Self {
            stack,
            inner: Position::Header { ranges, block_size },
            encoded,
            buf,
        }
    }

    pub fn buffer(&self) -> &[u8] {
        &self.buf
    }

    pub fn tree(&self) -> Option<&BaoTree> {
        match &self.inner {
            Position::Content { iter } => Some(iter.tree()),
            Position::Header { .. } => None,
        }
    }

    fn next0(&mut self) -> result::Result<Option<DecodeResponseItem>, DecodeError> {
        let inner = match &mut self.inner {
            Position::Content { ref mut iter } => iter,
            Position::Header {
                block_size,
                ranges: range,
            } => {
                let size = read_len(&mut self.encoded)?;
                // make sure the range is valid and canonical
                if !range_ok(range, size.chunks()) {
                    return Err(DecodeError::InvalidQueryRange);
                }
                let tree = BaoTree::new(size, *block_size);
                self.inner = Position::Content {
                    iter: tree.ranges_pre_order_chunks_iter_ref(range, 0),
                };
                return Ok(Some(Header { size }.into()));
            }
        };
        match inner.next() {
            Some(BaoChunk::Parent {
                is_root,
                left,
                right,
                node,
            }) => {
                let pair @ (l_hash, r_hash) = read_parent(&mut self.encoded)?;
                let parent_hash = self.stack.pop().unwrap();
                let actual = parent_cv(&l_hash, &r_hash, is_root);
                if parent_hash != actual {
                    return Err(DecodeError::ParentHashMismatch(node));
                }
                if right {
                    self.stack.push(r_hash);
                }
                if left {
                    self.stack.push(l_hash);
                }
                Ok(Some(Parent { node, pair }.into()))
            }
            Some(BaoChunk::Leaf {
                size,
                is_root,
                start_chunk,
            }) => {
                self.buf.resize(size, 0);
                self.encoded.read_exact(&mut self.buf)?;
                let actual = hash_block(start_chunk, &self.buf, is_root);
                let leaf_hash = self.stack.pop().unwrap();
                if leaf_hash != actual {
                    return Err(DecodeError::LeafHashMismatch(start_chunk));
                }
                Ok(Some(
                    Leaf {
                        offset: start_chunk.to_bytes(),
                        data: self.buf.split().freeze(),
                    }
                    .into(),
                ))
            }
            None => Ok(None),
        }
    }
}

impl<'a, R: Read> Iterator for DecodeResponseIter<'a, R> {
    type Item = result::Result<DecodeResponseItem, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next0().transpose()
    }
}

/// Encode ranges relevant to a query from a reader and outboard to a writer
///
/// This will not validate on writing, so data corruption will be detected on reading
pub fn encode_ranges<D: SliceReader, O: Outboard, W: Write>(
    data: D,
    outboard: O,
    ranges: &RangeSetRef<ChunkNum>,
    encoded: W,
) -> result::Result<(), EncodeError> {
    let mut data = data;
    let mut encoded = encoded;
    let file_len = ByteNum(data.len()?);
    let tree = outboard.tree();
    let ob_len = tree.size;
    if file_len != ob_len {
        return Err(EncodeError::SizeMismatch);
    }
    if !range_ok(ranges, tree.chunks()) {
        return Err(EncodeError::InvalidQueryRange);
    }
    let mut buffer = vec![0u8; tree.chunk_group_bytes().to_usize()];
    // write header
    encoded.write_all(tree.size.0.to_le_bytes().as_slice())?;
    for item in tree.ranges_pre_order_chunks_iter_ref(ranges, 0) {
        match item {
            BaoChunk::Parent { node, .. } => {
                let (l_hash, r_hash) = outboard.load(node)?.unwrap();
                encoded.write_all(l_hash.as_bytes())?;
                encoded.write_all(r_hash.as_bytes())?;
            }
            BaoChunk::Leaf {
                start_chunk, size, ..
            } => {
                let start = start_chunk.to_bytes();
                let buf = &mut buffer[..size];
                data.read_at(start.0, buf)?;
                encoded.write_all(buf)?;
            }
        }
    }
    Ok(())
}

/// Encode ranges relevant to a query from a reader and outboard to a writer
///
/// This function validates the data before writing
pub fn encode_ranges_validated<D: SliceReader, O: Outboard, W: Write>(
    data: D,
    outboard: O,
    ranges: &RangeSetRef<ChunkNum>,
    encoded: W,
) -> result::Result<(), EncodeError> {
    let mut stack = SmallVec::<[blake3::Hash; 10]>::new();
    stack.push(outboard.root());
    let mut data = data;
    let mut encoded = encoded;
    let file_len = ByteNum(data.len()?);
    let tree = outboard.tree();
    let ob_len = tree.size;
    if file_len != ob_len {
        return Err(EncodeError::SizeMismatch);
    }
    if !range_ok(ranges, tree.chunks()) {
        return Err(EncodeError::InvalidQueryRange);
    }
    let mut buffer = vec![0u8; tree.chunk_group_bytes().to_usize()];
    // write header
    encoded.write_all(tree.size.0.to_le_bytes().as_slice())?;
    for item in tree.ranges_pre_order_chunks_iter_ref(ranges, 0) {
        match item {
            BaoChunk::Parent {
                is_root,
                left,
                right,
                node,
            } => {
                let (l_hash, r_hash) = outboard.load(node)?.unwrap();
                let actual = parent_cv(&l_hash, &r_hash, is_root);
                let expected = stack.pop().unwrap();
                if actual != expected {
                    return Err(EncodeError::ParentHashMismatch(node));
                }
                if right {
                    stack.push(r_hash);
                }
                if left {
                    stack.push(l_hash);
                }
                encoded.write_all(l_hash.as_bytes())?;
                encoded.write_all(r_hash.as_bytes())?;
            }
            BaoChunk::Leaf {
                start_chunk,
                size,
                is_root,
            } => {
                let expected = stack.pop().unwrap();
                let start = start_chunk.to_bytes();
                let buf = &mut buffer[..size];
                data.read_at(start.0, buf)?;
                let actual = hash_block(start_chunk, buf, is_root);
                if actual != expected {
                    return Err(EncodeError::LeafHashMismatch(start_chunk));
                }
                encoded.write_all(buf)?;
            }
        }
    }
    Ok(())
}

/// Decode a response into a file while updating an outboard
pub async fn decode_response_into<R, O, W>(
    ranges: &RangeSetRef<ChunkNum>,
    encoded: R,
    mut outboard: O,
    mut target: W,
) -> io::Result<()>
where
    O: OutboardMut,
    R: Read,
    W: SliceWriter,
{
    let block_size = outboard.tree().block_size;
    let buffer = BytesMut::with_capacity(block_size.bytes());
    let iter = DecodeResponseIter::new(outboard.root(), block_size, encoded, ranges, buffer);
    for item in iter {
        match item? {
            DecodeResponseItem::Header(Header { size }) => {
                outboard.set_size(size)?;
            }
            DecodeResponseItem::Parent(Parent { node, pair }) => {
                outboard.save(node, &pair)?;
            }
            DecodeResponseItem::Leaf(Leaf { offset, data }) => {
                target.write_at(offset.0, &data)?;
            }
        }
    }
    Ok(())
}

/// Write ranges from memory to disk
///
/// This is useful for writing changes to outboards.
/// Note that it is up to you to call flush.
pub fn write_ranges(
    from: impl AsRef<[u8]>,
    mut to: impl Write + Seek,
    ranges: &RangeSetRef<u64>,
) -> io::Result<()> {
    let from = from.as_ref();
    let end = from.len() as u64;
    for range in ranges.iter() {
        let range = match range {
            RangeSetRange::RangeFrom(x) => *x.start..end,
            RangeSetRange::Range(x) => *x.start..*x.end,
        };
        let start = usize::try_from(range.start).unwrap();
        let end = usize::try_from(range.end).unwrap();
        to.seek(SeekFrom::Start(range.start))?;
        to.write_all(&from[start..end])?;
    }
    Ok(())
}

/// Compute the post order outboard for the given data, writing into a io::Write
pub fn outboard_post_order(
    data: &mut impl Read,
    size: u64,
    block_size: BlockSize,
    outboard: &mut impl Write,
) -> io::Result<blake3::Hash> {
    let tree = BaoTree::new_with_start_chunk(ByteNum(size), block_size, ChunkNum(0));
    let mut buffer = vec![0; tree.chunk_group_bytes().to_usize()];
    let hash = outboard_post_order_impl(tree, data, outboard, &mut buffer)?;
    outboard.write_all(&size.to_le_bytes())?;
    Ok(hash)
}

/// Compute the post order outboard for the given data
///
/// This is the internal version that takes a start chunk and does not append the size!
pub(crate) fn outboard_post_order_impl(
    tree: BaoTree,
    data: &mut impl Read,
    outboard: &mut impl Write,
    buffer: &mut [u8],
) -> io::Result<blake3::Hash> {
    // do not allocate for small trees
    let mut stack = SmallVec::<[blake3::Hash; 10]>::new();
    debug_assert!(buffer.len() == tree.chunk_group_bytes().to_usize());
    for item in tree.post_order_chunks_iter() {
        match item {
            BaoChunk::Parent { is_root, .. } => {
                let right_hash = stack.pop().unwrap();
                let left_hash = stack.pop().unwrap();
                outboard.write_all(left_hash.as_bytes())?;
                outboard.write_all(right_hash.as_bytes())?;
                let parent = parent_cv(&left_hash, &right_hash, is_root);
                stack.push(parent);
            }
            BaoChunk::Leaf {
                size,
                is_root,
                start_chunk,
            } => {
                let buf = &mut buffer[..size];
                data.read_exact(buf)?;
                let hash = hash_block(start_chunk, buf, is_root);
                stack.push(hash);
            }
        }
    }
    debug_assert_eq!(stack.len(), 1);
    let hash = stack.pop().unwrap();
    Ok(hash)
}

/// Internal hash computation. This allows to also compute a non root hash, e.g. for a block
///
/// Todo: maybe this should be just done recursively?
pub(crate) fn blake3_hash_inner(
    mut data: impl Read,
    data_len: ByteNum,
    start_chunk: ChunkNum,
    is_root: bool,
    buf: &mut [u8],
) -> std::io::Result<blake3::Hash> {
    let can_be_root = is_root;
    let mut stack = SmallVec::<[blake3::Hash; 10]>::new();
    let tree = BaoTree::new_with_start_chunk(data_len, BlockSize(0), start_chunk);
    for item in tree.post_order_chunks_iter() {
        match item {
            BaoChunk::Leaf {
                size,
                is_root,
                start_chunk,
            } => {
                let buf = &mut buf[..size];
                data.read_exact(buf)?;
                let hash = hash_chunk(start_chunk, buf, can_be_root && is_root);
                stack.push(hash);
            }
            BaoChunk::Parent { is_root, .. } => {
                let right_hash = stack.pop().unwrap();
                let left_hash = stack.pop().unwrap();
                let hash = parent_cv(&left_hash, &right_hash, can_be_root && is_root);
                stack.push(hash);
            }
        }
    }
    debug_assert_eq!(stack.len(), 1);
    Ok(stack.pop().unwrap())
}

fn read_len(from: &mut impl Read) -> std::io::Result<ByteNum> {
    let mut buf = [0; 8];
    from.read_exact(&mut buf)?;
    let len = ByteNum(u64::from_le_bytes(buf));
    Ok(len)
}

fn read_parent(from: &mut impl Read) -> std::io::Result<(blake3::Hash, blake3::Hash)> {
    let mut buf = [0; 64];
    from.read_exact(&mut buf)?;
    let l_hash = blake3::Hash::from(<[u8; 32]>::try_from(&buf[..32]).unwrap());
    let r_hash = blake3::Hash::from(<[u8; 32]>::try_from(&buf[32..]).unwrap());
    Ok((l_hash, r_hash))
}

/// seeks read the bytes for the range from the source
fn read_range<'a>(
    from: &mut (impl Read + Seek),
    range: Range<ByteNum>,
    buf: &'a mut [u8],
) -> std::io::Result<&'a [u8]> {
    let len = (range.end - range.start).to_usize();
    from.seek(std::io::SeekFrom::Start(range.start.0))?;
    let buf = &mut buf[..len];
    from.read_exact(buf)?;
    Ok(buf)
}

/// Given an outboard and a file, return all valid ranges
pub fn valid_file_ranges<O, R>(outboard: &O, reader: R) -> io::Result<RangeSet2<ChunkNum>>
where
    O: Outboard,
    R: Read + Seek,
{
    struct RecursiveValidator<'a, O: Outboard, R: Read + Seek> {
        tree: BaoTree,
        valid_nodes: TreeNode,
        res: RangeSet2<ChunkNum>,
        outboard: &'a O,
        reader: R,
        buffer: Vec<u8>,
    }

    impl<'a, O: Outboard, R: Read + Seek> RecursiveValidator<'a, O, R> {
        fn validate_rec(
            &mut self,
            parent_hash: &blake3::Hash,
            node: TreeNode,
            is_root: bool,
        ) -> io::Result<()> {
            if let Some((l_hash, r_hash)) = self.outboard.load(node)? {
                let actual = parent_cv(&l_hash, &r_hash, is_root);
                if &actual != parent_hash {
                    // we got a validation error. Simply continue without adding the range
                    return Ok(());
                }
                if let Some(leaf) = node.as_leaf() {
                    let (s, m, e) = self.tree.leaf_byte_ranges3(leaf);
                    let l_data = read_range(&mut self.reader, s..m, &mut self.buffer)?;
                    let actual = hash_block(s.chunks(), l_data, false);
                    if actual == l_hash {
                        self.res |= RangeSet2::from(s.chunks()..m.chunks());
                    }

                    let r_data = read_range(&mut self.reader, m..e, &mut self.buffer)?;
                    let actual = hash_block(m.chunks(), r_data, false);
                    if actual == r_hash {
                        self.res |= RangeSet2::from(m.chunks()..e.chunks());
                    }
                } else {
                    // recurse
                    let left = node.left_child().unwrap();
                    self.validate_rec(&l_hash, left, false)?;
                    let right = node.right_descendant(self.valid_nodes).unwrap();
                    self.validate_rec(&r_hash, right, false)?;
                }
            } else if let Some(leaf) = node.as_leaf() {
                let (s, m, _) = self.tree.leaf_byte_ranges3(leaf);
                let l_data = read_range(&mut self.reader, s..m, &mut self.buffer)?;
                let actual = hash_block(s.chunks(), l_data, is_root);
                if actual == *parent_hash {
                    self.res |= RangeSet2::from(s.chunks()..m.chunks());
                }
            };
            Ok(())
        }
    }
    let tree = outboard.tree();
    let root_hash = outboard.root();
    let mut validator = RecursiveValidator {
        tree,
        valid_nodes: tree.filled_size(),
        res: RangeSet2::empty(),
        outboard,
        reader,
        buffer: vec![0; tree.block_size.bytes()],
    };
    validator.validate_rec(&root_hash, tree.root(), true)?;
    Ok(validator.res)
}
