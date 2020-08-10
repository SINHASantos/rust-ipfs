use super::{
    CustomFlatUnixFs, DirBuilder, Entry, Leaf, NamedLeaf, TreeConstructionFailed, TreeOptions,
};
use cid::Cid;
use std::collections::HashMap;
use std::fmt;

/// Constructs the directory nodes required for a tree.
///
/// Implements the Iterator interface for owned values and the borrowed version, `next_borrowed`.
/// The tree is fully constructed once this has been exhausted.
pub struct PostOrderIterator {
    full_path: String,
    old_depth: usize,
    block_buffer: Vec<u8>,
    // our stack of pending work
    pending: Vec<Visited>,
    // "communication channel" from nested entries back to their parents; this hashmap is only used
    // in the event of mixed child nodes (leaves and nodes).
    persisted_cids: HashMap<u64, Vec<Option<NamedLeaf>>>,
    reused_children: Vec<Visited>,
    cid: Option<Cid>,
    total_size: u64,
    // from TreeOptions
    opts: TreeOptions,
}

/// The link list used to create the directory node. This list is created from a the BTreeMap
/// inside DirBuilder, and initially it will have `Some` values only for the initial leaves and
/// `None` values for subnodes which are not yet ready. At the time of use, this list is expected
/// to have only `Some` values.
type Leaves = Vec<Option<NamedLeaf>>;

/// The nodes in the visit. We need to do a post-order visit, which starts from a single
/// `DescentRoot`, followed by N `Descents` where N is the deepest directory in the tree. On each
/// descent, we'll need to first schedule a `Post` (or `PostRoot`) followed the immediate children
/// of the node. Directories are rendered when all of their direct and indirect descendants have
/// been serialized into NamedLeafs.
#[derive(Debug)]
enum Visited {
    // handle root differently not to infect with the Option<String> and Option<usize>
    DescentRoot(DirBuilder),
    Descent {
        node: DirBuilder,
        name: String,
        depth: usize,
        /// The index in the parents `Leaves` accessible through `PostOrderIterator::persisted_cids`.
        index: usize,
    },
    Post {
        parent_id: u64,
        depth: usize,
        name: String,
        index: usize,
        /// Leaves will be stored directly in this field when there are no DirBuilder descendants,
        /// in the `PostOrderIterator::persisted_cids` otherwise.
        leaves: LeafStorage,
    },
    PostRoot {
        leaves: LeafStorage,
    },
}

impl PostOrderIterator {
    pub(super) fn new(root: DirBuilder, opts: TreeOptions, longest_path: usize) -> Self {
        let root = Visited::DescentRoot(root);
        PostOrderIterator {
            full_path: String::with_capacity(longest_path),
            old_depth: 0,
            block_buffer: Default::default(),
            pending: vec![root],
            persisted_cids: Default::default(),
            reused_children: Vec::new(),
            cid: None,
            total_size: 0,
            opts,
        }
    }

    fn render_directory(
        links: &[Option<NamedLeaf>],
        buffer: &mut Vec<u8>,
        block_size_limit: &Option<u64>,
    ) -> Result<Leaf, TreeConstructionFailed> {
        use crate::pb::{UnixFs, UnixFsType};
        use quick_protobuf::{BytesWriter, MessageWrite, Writer};
        use sha2::{Digest, Sha256};

        // FIXME: ideas on how to turn this into a HAMT sharding on some heuristic. we probably
        // need to introduce states in to the "iterator":
        //
        // 1. bucketization
        // 2. another post order visit of the buckets?
        //
        // the nested post order visit should probably re-use the existing infra ("message
        // passing") and new ids can be generated by giving this iterator the counter from
        // BufferedTreeBuilder.
        //
        // could also be that the HAMT shard building should start earlier, since the same
        // heuristic can be detected *at* bufferedtreewriter. there the split would be easier, and
        // this would "just" be a single node rendering, and not need any additional states..

        let node = CustomFlatUnixFs {
            links,
            data: UnixFs {
                Type: UnixFsType::Directory,
                ..Default::default()
            },
        };

        let size = node.get_size();

        if let Some(limit) = block_size_limit {
            let size = size as u64;
            if *limit < size {
                // FIXME: this could probably be detected at builder
                return Err(TreeConstructionFailed::TooLargeBlock(size));
            }
        }

        let cap = buffer.capacity();

        if let Some(additional) = size.checked_sub(cap) {
            buffer.reserve(additional);
        }

        if let Some(mut needed_zeroes) = size.checked_sub(buffer.len()) {
            let zeroes = [0; 8];

            while needed_zeroes > 8 {
                buffer.extend_from_slice(&zeroes[..]);
                needed_zeroes -= zeroes.len();
            }

            buffer.extend(std::iter::repeat(0).take(needed_zeroes));
        }

        let mut writer = Writer::new(BytesWriter::new(&mut buffer[..]));
        node.write_message(&mut writer)
            .map_err(TreeConstructionFailed::Protobuf)?;

        buffer.truncate(size);

        let mh = multihash::wrap(multihash::Code::Sha2_256, &Sha256::digest(&buffer));
        let cid = Cid::new_v0(mh).expect("sha2_256 is the correct multihash for cidv0");

        let combined_from_links = links
            .iter()
            .map(|opt| {
                opt.as_ref()
                    .map(|NamedLeaf(_, _, total_size)| total_size)
                    .unwrap()
            })
            .sum::<u64>();

        Ok(Leaf {
            link: cid,
            total_size: buffer.len() as u64 + combined_from_links,
        })
    }

    /// Construct the next dag-pb node, if any.
    ///
    /// Returns a `TreeNode` of the latest constructed tree node.
    pub fn next_borrowed(&mut self) -> Option<Result<TreeNode<'_>, TreeConstructionFailed>> {
        while let Some(visited) = self.pending.pop() {
            let (name, depth) = match &visited {
                Visited::DescentRoot(_) => (None, 0),
                Visited::Descent { name, depth, .. } => (Some(name.as_ref()), *depth),
                Visited::Post { name, depth, .. } => (Some(name.as_ref()), *depth),
                Visited::PostRoot { .. } => (None, 0),
            };

            update_full_path((&mut self.full_path, &mut self.old_depth), name, depth);

            match visited {
                Visited::DescentRoot(node) => {
                    let children = &mut self.reused_children;
                    let leaves = partition_children_leaves(depth, node.nodes.into_iter(), children);
                    let any_children = !children.is_empty();

                    let leaves = if any_children {
                        self.persisted_cids.insert(node.id, leaves);
                        LeafStorage::from(node.id)
                    } else {
                        leaves.into()
                    };

                    self.pending.push(Visited::PostRoot { leaves });
                    self.pending.extend(children.drain(..));
                }
                Visited::Descent {
                    node,
                    name,
                    depth,
                    index,
                } => {
                    let children = &mut self.reused_children;
                    let leaves = partition_children_leaves(depth, node.nodes.into_iter(), children);
                    let any_children = !children.is_empty();
                    let parent_id = node.parent_id.expect("only roots parent_id is None");

                    let leaves = if any_children {
                        self.persisted_cids.insert(node.id, leaves);
                        node.id.into()
                    } else {
                        leaves.into()
                    };

                    self.pending.push(Visited::Post {
                        parent_id,
                        name,
                        depth,
                        leaves,
                        index,
                    });

                    self.pending.extend(children.drain(..));
                }
                Visited::Post {
                    parent_id,
                    name,
                    leaves,
                    index,
                    ..
                } => {
                    let leaves = leaves.into_inner(&mut self.persisted_cids);
                    let buffer = &mut self.block_buffer;

                    let leaf = match Self::render_directory(
                        &leaves,
                        buffer,
                        &self.opts.block_size_limit,
                    ) {
                        Ok(leaf) => leaf,
                        Err(e) => return Some(Err(e)),
                    };

                    self.cid = Some(leaf.link.clone());
                    self.total_size = leaf.total_size;

                    {
                        // name is None only for wrap_with_directory, which cannot really be
                        // propagated up but still the parent_id is allowed to be None
                        let parent_leaves = self.persisted_cids.get_mut(&parent_id);

                        match (parent_id, parent_leaves, index) {
                            (pid, None, index) => panic!(
                                "leaves not found for parent_id = {} and index = {}",
                                pid, index
                            ),
                            (_, Some(vec), index) => {
                                let cell = &mut vec[index];
                                // all
                                assert!(cell.is_none());
                                *cell = Some(NamedLeaf(name, leaf.link, leaf.total_size));
                            }
                        }
                    }

                    return Some(Ok(TreeNode {
                        path: self.full_path.as_str(),
                        cid: self.cid.as_ref().unwrap(),
                        total_size: self.total_size,
                        block: &self.block_buffer,
                    }));
                }
                Visited::PostRoot { leaves } => {
                    let leaves = leaves.into_inner(&mut self.persisted_cids);

                    if !self.opts.wrap_with_directory {
                        break;
                    }

                    let buffer = &mut self.block_buffer;

                    let leaf = match Self::render_directory(
                        &leaves,
                        buffer,
                        &self.opts.block_size_limit,
                    ) {
                        Ok(leaf) => leaf,
                        Err(e) => return Some(Err(e)),
                    };

                    self.cid = Some(leaf.link.clone());
                    self.total_size = leaf.total_size;

                    return Some(Ok(TreeNode {
                        path: self.full_path.as_str(),
                        cid: self.cid.as_ref().unwrap(),
                        total_size: self.total_size,
                        block: &self.block_buffer,
                    }));
                }
            }
        }
        None
    }
}

impl Iterator for PostOrderIterator {
    type Item = Result<OwnedTreeNode, TreeConstructionFailed>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_borrowed()
            .map(|res| res.map(TreeNode::into_owned))
    }
}

/// Borrowed representation of a node in the tree.
pub struct TreeNode<'a> {
    /// Full path to the node.
    pub path: &'a str,
    /// The Cid of the document.
    pub cid: &'a Cid,
    /// Cumulative total size of the subtree in bytes.
    pub total_size: u64,
    /// Raw dag-pb document.
    pub block: &'a [u8],
}

impl<'a> fmt::Debug for TreeNode<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("TreeNode")
            .field("path", &format_args!("{:?}", self.path))
            .field("cid", &format_args!("{}", self.cid))
            .field("total_size", &self.total_size)
            .field("size", &self.block.len())
            .finish()
    }
}

impl TreeNode<'_> {
    /// Convert to an owned and detached representation.
    pub fn into_owned(self) -> OwnedTreeNode {
        OwnedTreeNode {
            path: self.path.to_owned(),
            cid: self.cid.to_owned(),
            total_size: self.total_size,
            block: self.block.into(),
        }
    }
}

/// Owned representation of a node in the tree.
pub struct OwnedTreeNode {
    /// Full path to the node.
    pub path: String,
    /// The Cid of the document.
    pub cid: Cid,
    /// Cumulative total size of the subtree in bytes.
    pub total_size: u64,
    /// Raw dag-pb document.
    pub block: Box<[u8]>,
}

fn update_full_path(
    (full_path, old_depth): (&mut String, &mut usize),
    name: Option<&str>,
    depth: usize,
) {
    if depth < 2 {
        // initially thought it might be a good idea to add a slash to all components; removing it made
        // it impossible to get back down to empty string, so fixing this for depths 0 and 1.
        full_path.clear();
        *old_depth = 0;
    } else {
        while *old_depth >= depth && *old_depth > 0 {
            // we now want to pop the last segment
            // this would be easier with PathBuf
            let slash_at = full_path.bytes().rposition(|ch| ch == b'/');
            if let Some(slash_at) = slash_at {
                if *old_depth == depth && Some(&full_path[(slash_at + 1)..]) == name {
                    // minor unmeasurable perf optimization:
                    // going from a/b/foo/zz => a/b/foo does not need to go through the a/b
                    return;
                }
                full_path.truncate(slash_at);
                *old_depth -= 1;
            } else {
                todo!(
                    "no last slash_at in {:?} yet {} >= {}",
                    full_path,
                    old_depth,
                    depth
                );
            }
        }
    }

    debug_assert!(*old_depth <= depth);

    if let Some(name) = name {
        if !full_path.is_empty() {
            full_path.push_str("/");
        }
        full_path.push_str(name);
        *old_depth += 1;
    }

    assert_eq!(*old_depth, depth);
}

/// Returns a Vec of the links in order with only the leaves, the given `children` will contain yet
/// incomplete nodes of the tree.
fn partition_children_leaves(
    depth: usize,
    it: impl Iterator<Item = (String, Entry)>,
    children: &mut Vec<Visited>,
) -> Leaves {
    let mut leaves = Vec::new();

    for (i, (k, v)) in it.enumerate() {
        match v {
            Entry::Directory(node) => {
                children.push(Visited::Descent {
                    node,
                    // this needs to be pushed down to update the full_path
                    name: k,
                    depth: depth + 1,
                    index: i,
                });

                // this will be overwritten later, but the order is fixed
                leaves.push(None);
            }
            Entry::Leaf(leaf) => leaves.push(Some(NamedLeaf(k, leaf.link, leaf.total_size))),
        }
    }

    leaves
}

#[derive(Debug)]
enum LeafStorage {
    Direct(Leaves),
    Stashed(u64),
}

impl LeafStorage {
    fn into_inner(self, stash: &mut HashMap<u64, Leaves>) -> Leaves {
        use LeafStorage::*;

        match self {
            Direct(leaves) => leaves,
            Stashed(id) => stash
                .remove(&id)
                .ok_or(id)
                .expect("leaves are either stashed or direct, must able to find with id"),
        }
    }
}

impl From<u64> for LeafStorage {
    fn from(key: u64) -> LeafStorage {
        LeafStorage::Stashed(key)
    }
}

impl From<Leaves> for LeafStorage {
    fn from(leaves: Leaves) -> LeafStorage {
        LeafStorage::Direct(leaves)
    }
}
