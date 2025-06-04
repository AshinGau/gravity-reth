use alloy_primitives::B256;
use alloy_trie::EMPTY_ROOT_HASH;
use nybbles::Nibbles;

use crate::nested_trie::node::{Node, NodeFlag};

pub trait TrieReader {
    fn read(&self, path: &Nibbles) -> Option<Node>;
}

pub trait TrieWriter {
    fn write(&self, path: &Nibbles, node: &Node);
}

pub struct Trie<R, W>
where
    R: TrieReader,
    W: TrieWriter,
{
    root: Option<Node>,
    reader: R,
    writer: W,
}

impl<R, W> Trie<R, W>
where
    R: TrieReader,
    W: TrieWriter,
{
    pub fn new(root: Option<Node>, reader: R, writer: W) -> Self {
        Self { root, reader, writer }
    }

    fn insert_inner(&self, node: Option<Node>, prefix: Nibbles, key: Nibbles, value: Node) -> (bool, Node) {
        if key.is_empty() {
            return (true, value);
        }

        if let Some(node) = node {
            match node {
                Node::FullNode { mut children, mut flags } => {
                    let index = key[0] as usize;
                    let child = children[index].take().map(|n| *n);
                    let mut new_prefix = prefix.clone();
                    new_prefix.push_unchecked(key[0]);
                    let (dirty, new_node) = self.insert_inner(child, new_prefix, key.slice(1..), value);
                    children[index] = Some(Box::new(new_node));
                    if dirty {
                        flags.mark_diry();
                    }
                    (dirty, Node::FullNode { children, flags })
                },
                Node::ShortNode { key: node_key, value: node_value, mut flags } => {
                    // If the whole key matches, keep this short node as is
		            // and only update the value.
                    let matchlen = key.common_prefix_length(&node_key);
                    if matchlen == node_key.len() {
                        let next_node = Some(*node_value);
                        let mut new_prefix = prefix.clone();
                        new_prefix.extend_from_slice_unchecked(&key[..matchlen]);
                        let (dirty, new_node) = self.insert_inner(next_node, new_prefix, key.slice(matchlen..), value);
                        if dirty {
                            flags.mark_diry();
                        }
                        return (dirty, Node::ShortNode { key: node_key, value: Box::new(new_node), flags });
                    }
                    // Otherwise branch out at the index where they differ.
                    let mut children: [Option<Box<Node>>; 17] = Default::default();
                    let matchlen_inc = matchlen + 1;
                    let mut new_prefix = prefix.clone();
                    new_prefix.extend_from_slice_unchecked(&node_key[..matchlen_inc]);
                    let (_, extension_node) = self.insert_inner(None, new_prefix, node_key.slice(matchlen_inc..), *node_value);
                    children[node_key[matchlen] as usize] = Some(Box::new(extension_node));

                    let mut new_prefix = prefix.clone();
                    // if matchlen == key.len(), value is the value of the branch node.
                    new_prefix.extend_from_slice_unchecked(&key[..matchlen_inc]);
                    let (_, new_node) = self.insert_inner(None, new_prefix, key.slice(matchlen_inc..), value);
                    children[key[matchlen] as usize] = Some(Box::new(new_node));

                    let branch = Node::FullNode { children, flags: NodeFlag::dirty_node() };
                    if matchlen == 0 {
                        (true, branch)
                    } else {
                        (true, Node::ShortNode { key: key.slice(..matchlen), value: Box::new(branch), flags: NodeFlag::dirty_node() })
                    }
                }
                Node::HashNode(rlp_node) => {
                    let real_node = self.reader.read(&prefix).unwrap();
                    let (dirty, new_node) = self.insert_inner(Some(real_node), prefix, key, value);
                    if dirty {
                        (true, new_node)
                    } else {
                        (false, Node::HashNode(rlp_node))
                    }
                },
                _ => unreachable!(),
            }
        } else {
            (true, Node::ShortNode { key, value: Box::new(value), flags: NodeFlag::dirty_node() })
        }
    }

    pub fn insert(&mut self, key: Nibbles, value: Node) {
        let root = self.root.take();
        let (_, new_root) = self.insert_inner(root, Nibbles::new(), key, value);
        self.root = Some(new_root);
    }

    pub fn delete(&mut self, key: Nibbles) {
        let root = self.root.take();
        let (_, new_root) = self.delete_inner(root, Nibbles::new(), key);
        self.root = new_root;
    }

    fn delete_inner(&self, node: Option<Node>, prefix: Nibbles, key: Nibbles) -> (bool, Option<Node>) {
        if let Some(node) = node {
            match node {
                Node::FullNode { mut children, flags } => {
                    let index = key[0] as usize;
                    let child = children[index].take().map(|n| *n);
                    let mut new_prefix = prefix.clone();
                    new_prefix.push_unchecked(key[0]);
                    let (dirty, new_node) = self.delete_inner(child, new_prefix, key.slice(1..));
                    children[index] = new_node.map(|n| Box::new(n));
                    if !dirty {
                        return (false, Some(Node::FullNode { children, flags }));
                    }
                    
                    // Because n is a full node, it must've contained at least two children
                    // before the delete operation. If the new child value is non-nil, n still
                    // has at least two children after the deletion, and cannot be reduced to
                    // a short node.
                    if children[index].is_some() {
                        return (true, Some(Node::FullNode { children, flags: NodeFlag::dirty_node() }));
                    }

                    // Reduction:
                    // Check how many non-nil entries are left after deleting and
                    // reduce the full node to a short node if only one entry is
                    // left. Since n must've contained at least two children
                    // before deletion (otherwise it would not be a full node) n
                    // can never be reduced to nil.
                    //
                    // When the loop is done, pos contains the index of the single
                    // value that is left in n or -2 if n contains at least two
                    // values.
                    let mut pos = -1;
                    for (i, child) in children.iter().enumerate() {
                        if child.is_some() {
                            if pos == -1 {
                                pos = i as i32;
                            } else {
                                pos = -2;
                                break;
                            }
                        }
                    }
                    // pos can't be -2
                    if pos >= 0 {
                        let mut single_child = *children[pos as usize].take().unwrap();
                        if pos != 16 {
                            // If the remaining entry is a short node, it replaces
                            // n and its key gets the missing nibble tacked to the
                            // front. This avoids creating an invalid
                            // shortNode{..., shortNode{...}}.  Since the entry
                            // might not be loaded yet, resolve it just for this
                            // check.
                            let mut child_path = prefix.clone();
                            child_path.push_unchecked(pos as u8);
                            single_child = self.resolve(single_child, child_path);
                        }
                        if let Node::ShortNode { key: cn_key, value: cn_value, .. } = single_child {
                            // Replace the entire full node with the short node.
                            // Mark the original short node as deleted since the
                            // value is embedded into the parent now.
                            let mut new_key = Nibbles::from_nibbles_unchecked([pos as u8]);
                            new_key.extend_from_slice_unchecked(&cn_key);
                            (true, Some(Node::ShortNode { key: new_key, value: cn_value, flags: NodeFlag::dirty_node() }))
                        } else {
                            // Otherwise, n is replaced by a one-nibble short node
			                // containing the child.
                            let new_key = Nibbles::from_nibbles_unchecked([pos as u8]);
                            (true, Some(Node::ShortNode { key: new_key, value: Box::new(single_child), flags: NodeFlag::dirty_node() }))
                        }
                    } else {
                        (true, Some(Node::FullNode { children, flags: NodeFlag::dirty_node() }))
                    }
                },
                Node::ShortNode { key: node_key, value: node_value, flags } => {
                    let matchlen = key.common_prefix_length(&node_key);
                    if matchlen < node_key.len() {
                        // don't replace n on mismatch
                        return (false, Some(Node::ShortNode { key: node_key, value: node_value, flags }));
                    }
                    if matchlen == key.len() {
                        // The matched short node is deleted entirely and track
                        // it in the deletion set. The same the valueNode doesn't
                        // need to be tracked at all since it's always embedded.
                        return (true, None); // // remove n entirely for whole matches
                    }
                    let next_node = Some(*node_value);
                    let mut new_prefix = prefix.clone();
                    new_prefix.extend_from_slice_unchecked(&key[..matchlen]);
                    let (dirty, new_node) = self.delete_inner(next_node, new_prefix, key.slice(matchlen..));
                    let new_node = new_node.unwrap();
                    if !dirty {
                        return (false, Some(Node::ShortNode { key: node_key, value: Box::new(new_node), flags }));
                    }
                    match new_node {
                        Node::ShortNode { key: child_key, value: child_value, .. } => {
                            let mut extend_key = node_key.clone();
                            extend_key.extend_from_slice_unchecked(&child_key);
                            (true, Some(Node::ShortNode { key: extend_key, value: child_value, flags: NodeFlag::dirty_node() }))
                        },
                        _ => {
                            (true, Some(Node::ShortNode { key: node_key, value: Box::new(new_node), flags: NodeFlag::dirty_node() }))
                        }
                    }
                },
                Node::ValueNode(as_ref) => {
                    (true, None)
                },
                Node::HashNode(rlp_node) => {
                    let real_node = self.reader.read(&prefix);
                    let (dirty, new_node) = self.delete_inner(real_node, prefix, key);
                    if dirty {
                        (true, new_node)
                    } else {
                        (false, Some(Node::HashNode(rlp_node)))
                    }
                },
            }
        } else {
            (false, None)
        }
    }

    fn resolve(&self, node: Node, path: Nibbles) -> Node {
        if let Node::HashNode(..) = &node {
            self.reader.read(&path).unwrap()
        } else {
            node
        }
    }

    pub fn hash(&mut self) -> B256 {
        if let Some(root) = &mut self.root {
            let mut buf = Vec::new();
            root.build_hash(&mut buf);
            root.hash()
        } else {
            EMPTY_ROOT_HASH
        }
    }
}
