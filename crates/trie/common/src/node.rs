use alloy_primitives::{keccak256, B256};
use alloy_rlp::{BufMut, Decodable, Encodable, Header, EMPTY_STRING_CODE};
use alloy_trie::nodes::RlpNode;
use nybbles::Nibbles;

enum NodeType {
    BranchNode, ExtensionNode, LeafNode, HashNode, ValueNode
}

#[derive(Default)]
struct NodeFlag {
    hash: Option<Box<dyn AsRef<[u8]>>>,
    dirty: bool,
}

impl NodeFlag {
    fn new(hash: Option<Box<dyn AsRef<[u8]>>>, dirty: bool) -> Self {
        Self {hash, dirty}
    }

    fn dirty_node() -> Self {
        Self { hash: None, dirty: true }
    }

    fn flags(&self) -> (Option<&[u8]>, bool) {
        (self.hash.as_ref().map(|f| f.as_ref().as_ref()), self.dirty)
    }
}

trait Node {
    fn node_type(&self) -> NodeType;

    fn cache(&self) -> (Option<&[u8]>, bool);

    fn rlp_payload_length(&mut self, buffer: &mut Vec<u8>) -> usize;

    fn rlp(&mut self, buffer: &mut Vec<u8>);

    fn encode(&mut self, out: &mut Vec<u8>);

    fn insert(&mut self, prefix: Nibbles, key: Nibbles, value: ValueNode);

    fn delete(&mut self, prefix: Nibbles, key: Nibbles);
}

// impl serde::Serialize, serde::Deserialize for storage.
struct NodeRef(Box<dyn Node>);

struct BranchNode {
    children: [Option<Box<dyn Node>>; 17],
    flags: NodeFlag,
}

struct ExtensionNode {
    shared_nibbles: Box<dyn AsRef<[u8]>>,
    next_node: Option<Box<dyn Node>>,
    flags: NodeFlag,
}

struct LeafNode {
    key_end: Box<dyn AsRef<[u8]>>,
    value: Option<Box<dyn Node>>,
    flags: NodeFlag,
}

struct HashNode(Box<dyn AsRef<[u8]>>);
struct ValueNode(Box<dyn AsRef<[u8]>>);

impl Node for BranchNode {
    fn node_type(&self) -> NodeType {
        NodeType::BranchNode
    }

    fn cache(&self) -> (Option<&[u8]>, bool) {
        self.flags.flags()
    }

    fn rlp_payload_length(&mut self, buffer: &mut Vec<u8>) -> usize {
        todo!()
    }

    fn rlp(&mut self, buffer: &mut Vec<u8>) {
        if self.flags.hash.is_some() {
            return;
        }
        let header = Header { list: true, payload_length: self.rlp_payload_length(buffer) };
        buffer.clear();
        header.encode(buffer);
        for child in &self.children {
            if let Some(child) = child {
                buffer.put_slice(self.cache().0.unwrap());
            } else {
                buffer.put_u8(EMPTY_STRING_CODE);
            }
        }
        let rlp = RlpNode::from_rlp(buffer);
        self.flags.hash = Some(Box::new(rlp.clone()));
    }
}

