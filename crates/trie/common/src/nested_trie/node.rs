use alloy_primitives::{keccak256, B256};
use alloy_rlp::{length_of_length, BufMut, Decodable, Encodable, Header, EMPTY_STRING_CODE};
use alloy_trie::{nodes::{encode_path_leaf, RlpNode}, EMPTY_ROOT_HASH};
use nybbles::Nibbles;

#[derive(Default, Debug)]
pub struct NodeFlag {
    rlp: Option<RlpNode>,
    dirty: bool,
}

impl NodeFlag {
    pub fn new(rlp: Option<RlpNode>) -> Self {
        Self { rlp, dirty: false }
    }

    pub fn dirty_node() -> Self {
        Self { rlp: None, dirty: true }
    }

    pub fn mark_diry(&mut self) {
        self.rlp.take();
        self.dirty = true;
    }
}

pub enum Node {
    FullNode {
        children: [Option<Box<Node>>; 17],
        flags: NodeFlag,
    },
    ShortNode {
        key: Nibbles,
        value: Box<Node>,
        flags: NodeFlag,
    },
    ValueNode(Box<dyn AsRef<[u8]>>),
    HashNode(RlpNode),
}

impl Node {
    pub fn cached_rlp(&self) -> Option<&RlpNode> {
        match self {
            Node::FullNode { children: _, flags } => flags.rlp.as_ref(),
            Node::ShortNode { key: _, value: _, flags } => flags.rlp.as_ref(),
            Node::ValueNode(_) => None,
            Node::HashNode(rlp_node) => Some(&rlp_node),
        }
    }

    pub fn dirty(&self) -> bool {
        match self {
            Node::FullNode { children: _, flags } => flags.dirty,
            Node::ShortNode { key: _, value: _, flags } => flags.dirty,
            Node::ValueNode(_) => true,
            Node::HashNode(_) => false,
        }
    }

    pub fn build_hash(&mut self, buf: &mut Vec<u8>) -> &RlpNode {
        match self {
            Node::FullNode { children, flags } => {
                if flags.rlp.is_none() {
                    let header = Header { list: true, payload_length: branch_node_rlp_length(children, buf) };
                    buf.clear();
                    header.encode(buf);
                    for child in children {
                        if let Some(child) = child {
                            buf.put_slice(child.cached_rlp().unwrap());
                        } else {
                            buf.put_u8(EMPTY_STRING_CODE);
                        }
                    }
                    flags.rlp = Some(RlpNode::from_rlp(buf));
                }
                flags.rlp.as_ref().unwrap()
            },
            Node::ShortNode { key, value, flags } => {
                if flags.rlp.is_none() {
                    if let Node::ValueNode(value) = value.as_ref() {
                        let header = Header { list: true, payload_length: leaf_node_rlp_length(key, value) };
                        buf.clear();
                        header.encode(buf);
                        encode_path_leaf(key, true).as_slice().encode(buf);
                        Encodable::encode(value.as_ref().as_ref(), buf);
                    } else {
                        let header = Header { list: true, payload_length: extension_node_rlp_length(key, value, buf) };
                        buf.clear();
                        header.encode(buf);
                        encode_path_leaf(key, false).as_slice().encode(buf);
                        buf.put_slice(value.cached_rlp().unwrap());
                    }
                    flags.rlp = Some(RlpNode::from_rlp(buf));
                }
                flags.rlp.as_ref().unwrap()
            },
            Node::HashNode(rlp_node) => rlp_node,
            _ => unreachable!(),
        }
    }

    pub fn hash(&self) -> B256 {
        if let Some(node_ref) = self.cached_rlp() {
            if let Some(hash) = node_ref.as_hash() {
                hash
            } else {
                keccak256(node_ref)
            }
        } else {
            panic!("build hash first!");
        }
    }
}

/// Returns the length of RLP encoded fields of branch node.
fn branch_node_rlp_length(children: &mut [Option<Box<Node>>; 17], buf: &mut Vec<u8>) -> usize {
    let mut payload_length = 0;
    for child in children {
        if let Some(child) = child {
            if let Some(rlp) = child.cached_rlp() {
                payload_length += rlp.len();
            } else {
                payload_length += child.build_hash(buf).len();
            }
        } else {
            payload_length += 1;
        }
    }
    payload_length
}

/// Returns the length of RLP encoded fields of extension node.
fn extension_node_rlp_length(shared_nibbles: &Nibbles, next_node: &mut Box<Node>, buf: &mut Vec<u8>) -> usize {
    let mut encoded_key_len = shared_nibbles.len() / 2 + 1;
    // For extension nodes the first byte cannot be greater than 0x80.
    if encoded_key_len != 1 {
        encoded_key_len += length_of_length(encoded_key_len);
    }
    if let Some(rlp) = next_node.cached_rlp() {
        encoded_key_len += rlp.len();
    } else {
        encoded_key_len += next_node.build_hash(buf).len();
    }
    encoded_key_len
}

/// Returns the length of RLP encoded fields of leaf node.
fn leaf_node_rlp_length(key_end: &Nibbles, value: &Box<dyn AsRef<[u8]>>) -> usize {
    let mut encoded_key_len = key_end.len() / 2 + 1;
    // For leaf nodes the first byte cannot be greater than 0x80.
    if encoded_key_len != 1 {
        encoded_key_len += length_of_length(encoded_key_len);
    }
    encoded_key_len + Encodable::length(value.as_ref().as_ref())
}
