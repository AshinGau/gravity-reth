use alloy_primitives::B256;
use alloy_trie::EMPTY_ROOT_HASH;
use nybbles::Nibbles;

use crate::nested_trie::node::{Node, NodeFlag};

trait TrieReader {
    fn read(&self, path: &Nibbles) -> Node;
}

trait TrieWriter {
    fn write(&self, path: &Nibbles, node: &Node);
}

struct Trie<R, W>
where
    R: TrieReader,
    W: TrieWriter,
{
    root: Option<Node>,
    reader: R,
    commiter: W,
}


impl<R, W> Trie<R, W>
where
    R: TrieReader,
    W: TrieWriter,
{
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
                        flags.mark_diry();
                        (true, Node::ShortNode { key: key.slice(..matchlen), value: Box::new(branch), flags })
                    }
                }
                Node::HashNode(rlp_node) => {
                    let real_node = self.reader.read(&prefix);
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

    pub fn insert(&mut self, prefix: Nibbles, key: Nibbles, value: Node) {
        let root = self.root.take();
        let (_, new_root) = self.insert_inner(root, prefix, key, value);
        self.root = Some(new_root);
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
