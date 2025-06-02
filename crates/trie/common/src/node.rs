use alloy_rlp::{BufMut, Encodable, Header};

enum Node {
    FullNode {
        children: [Option<Box<Node>>; 17],
        flags: NodeFlag,
    },
    ShortNode {
        key: Box<dyn AsRef<[u8]>>,
        val: Option<Box<Node>>,
        flags: NodeFlag,
    },
    HashNode(Box<dyn AsRef<[u8]>>),
    ValueNode(Box<dyn AsRef<[u8]>>),
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

impl Node {
    fn cache(&self) -> (Option<&[u8]>, bool) {
        match self {
            Node::FullNode { children: _, flags } => flags.flags(),
            Node::ShortNode { key: _, val: _, flags } => flags.flags(),
            _ => (None, true),
        }
    }

    /// Returns the length of RLP encoded fields of node.
    #[inline]
    fn rlp_payload_length(&self) -> usize {
        match self {
            Node::FullNode { children, flags } => todo!(),
            Node::ShortNode { key, val, flags } => todo!(),
            Node::HashNode(items) => todo!(),
            Node::ValueNode(items) => todo!(),
        }
    }
}

impl Encodable for Node {
    #[inline]
    fn encode(&self, out: &mut dyn BufMut) {
        match self {
            Node::FullNode { children, flags } => {
                Header { list: true, payload_length: self.rlp_payload_length() }.encode(out);
                for child in children {
                    if let Some(child) = child {
                        
                    } else {

                    }
                }
                
            },
            Node::ShortNode { key, val, flags } => todo!(),
            Node::HashNode(as_ref) => todo!(),
            Node::ValueNode(as_ref) => todo!(),
        }
    }

    #[inline]
    fn length(&self) -> usize {
        todo!()
    }
}
