use nybbles::Nibbles;

use crate::nested_trie::node::{Node, NodeFlag};

trait TrieReader {
    fn read(&self, path: Nibbles) -> Node;
}

trait TrieWriter {
    fn write(&self, path: Nibbles, node: Node);
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
    fn insert_inner(node: &mut Node, prefix: &Nibbles, value: Box<dyn AsRef<[u8]>>) -> (bool, Node) {
        match node {
            Node::BranchNode { children, flags } => todo!(),
            Node::ExtensionNode { shared_nibbles, next_node, flags } => todo!(),
            Node::LeafNode { key_end, value, flags } => todo!(),
            Node::HashNode(rlp_node) => todo!(),
        }
    }

    pub fn insert(&mut self, prefix: Nibbles, value: Box<dyn AsRef<[u8]>>) -> (bool, Node) {
        if self.root.is_none() {
            self.root = Some(Node::LeafNode { key_end: prefix, value, flags: NodeFlag::dirty_node() });

        }
    }

}
