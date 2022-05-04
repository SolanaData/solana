use {
    rayon::{iter::ParallelIterator, prelude::*},
    serde::{Deserialize, Serialize},
    sha2::{Digest, Sha256},
};

pub const TURBINE_MERKLE_HASH_BYTES: usize = 20;
pub const TURBINE_MERKLE_ROOT_BYTES: usize = TURBINE_MERKLE_HASH_BYTES;
pub const TURBINE_MERKLE_PROOF_LENGTH_FEC64: usize = 6;
pub const TURBINE_MERKLE_PROOF_BYTES_FEC64: usize =
    TURBINE_MERKLE_HASH_BYTES * TURBINE_MERKLE_PROOF_LENGTH_FEC64;

#[repr(transparent)]
#[derive(Default, Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TurbineMerkleHash(pub [u8; TURBINE_MERKLE_HASH_BYTES]);

#[repr(transparent)]
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurbineMerkleProof(pub Vec<TurbineMerkleHash>);

#[repr(transparent)]
#[derive(Default, Copy, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurbineMerkleProofFec64(pub [TurbineMerkleHash; TURBINE_MERKLE_PROOF_LENGTH_FEC64]);

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurbineMerkleTree {
    tree: Vec<TurbineMerkleHash>,
}

impl TurbineMerkleHash {
    pub fn hash<T>(bufs: &[T]) -> TurbineMerkleHash
    where
        T: AsRef<[u8]> + Sync,
    {
        let mut hasher = Sha256::new();
        bufs.iter().for_each(|b| hasher.update(b));
        let h = hasher.finalize();
        let mut ret = TurbineMerkleHash::default();
        ret.0[..].copy_from_slice(&h.as_slice()[0..TURBINE_MERKLE_HASH_BYTES]);
        ret
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<&[u8]> for TurbineMerkleHash {
    fn from(buf: &[u8]) -> Self {
        assert!(buf.len() == TURBINE_MERKLE_HASH_BYTES);
        TurbineMerkleHash(buf.try_into().unwrap())
    }
}

impl AsRef<[u8]> for TurbineMerkleHash {
    fn as_ref(&self) -> &[u8] {
        &self.0[..]
    }
}

impl TurbineMerkleProof {
    #[allow(clippy::integer_arithmetic)]
    fn generic_compute_root(
        proof: &[TurbineMerkleHash],
        leaf_hash: &TurbineMerkleHash,
        leaf_index: usize,
    ) -> TurbineMerkleHash {
        let mut hash = *leaf_hash;
        let mut idx = leaf_index;
        for elem in proof {
            hash = if idx & 1 == 0 {
                TurbineMerkleHash::hash(&[&hash.0, &elem.0])
            } else {
                TurbineMerkleHash::hash(&[&elem.0, &hash.0])
            };
            idx >>= 1;
        }
        hash
    }

    fn generic_verify(
        proof: &[TurbineMerkleHash],
        root_hash: &TurbineMerkleHash,
        leaf_hash: &TurbineMerkleHash,
        leaf_index: usize,
    ) -> bool {
        &Self::generic_compute_root(proof, leaf_hash, leaf_index) == root_hash
    }

    pub fn compute_root(
        &self,
        leaf_hash: &TurbineMerkleHash,
        leaf_index: usize,
    ) -> TurbineMerkleHash {
        Self::generic_compute_root(&self.0, leaf_hash, leaf_index)
    }

    pub fn verify(
        &self,
        root_hash: &TurbineMerkleHash,
        leaf_hash: &TurbineMerkleHash,
        leaf_index: usize,
    ) -> bool {
        Self::generic_verify(&self.0, root_hash, leaf_hash, leaf_index)
    }
}

impl From<&[u8]> for TurbineMerkleProof {
    #[allow(clippy::integer_arithmetic)]
    fn from(buf: &[u8]) -> Self {
        assert!(buf.len() % TURBINE_MERKLE_HASH_BYTES == 0);
        let v: Vec<TurbineMerkleHash> = buf
            .chunks_exact(TURBINE_MERKLE_HASH_BYTES)
            .map(|x| x.into())
            .collect();
        TurbineMerkleProof(v)
    }
}

impl TurbineMerkleProofFec64 {
    pub fn compute_root(
        &self,
        leaf_hash: &TurbineMerkleHash,
        leaf_index: usize,
    ) -> TurbineMerkleHash {
        TurbineMerkleProof::generic_compute_root(&self.0, leaf_hash, leaf_index)
    }

    pub fn verify(
        &self,
        root_hash: &TurbineMerkleHash,
        leaf_hash: &TurbineMerkleHash,
        leaf_index: usize,
    ) -> bool {
        TurbineMerkleProof::generic_verify(&self.0, root_hash, leaf_hash, leaf_index)
    }
}

impl From<&[u8]> for TurbineMerkleProofFec64 {
    fn from(buf: &[u8]) -> Self {
        assert!(buf.len() == TURBINE_MERKLE_PROOF_BYTES_FEC64);
        TurbineMerkleProofFec64(
            TurbineMerkleProof::from(buf)
                .0
                .try_into()
                .expect("expect 6 elements"),
        )
    }
}

impl TurbineMerkleTree {
    #[allow(clippy::integer_arithmetic)]
    pub fn new_from_leaves(leaves: &[TurbineMerkleHash]) -> Self {
        // TODO assert leaves.len() is a power of 2
        let tree_size = leaves.len() * 2 - 1;
        let mut tree = Vec::with_capacity(tree_size);

        for leaf in leaves {
            tree.push(*leaf);
        }

        let mut base = 0;
        let mut level_leaves = leaves.len();
        while level_leaves > 1 {
            for i in (0..level_leaves).step_by(2) {
                let hash = TurbineMerkleHash::hash(&[&tree[base + i].0, &tree[base + i + 1].0]);
                tree.push(hash);
            }
            base += level_leaves;
            level_leaves >>= 1;
        }

        Self { tree }
    }

    pub fn new_from_bufs<T>(bufs: &[T]) -> Self
    where
        T: Sync + AsRef<[u8]>,
    {
        // TODO assert bufs.len() is power of 2 or pad to power of 2
        let leaves: Vec<_> = bufs
            .iter()
            .map(|b| TurbineMerkleHash::hash(&[b.as_ref()]))
            .collect();
        Self::new_from_leaves(&leaves)
    }

    #[allow(clippy::integer_arithmetic)]
    pub fn new_from_bufs_par<T>(bufs: &[T], chunk: usize) -> Self
    where
        T: Sync + AsRef<[u8]>,
    {
        // compute subtrees of chunk width in parallel
        let sub_trees: Vec<Vec<TurbineMerkleHash>> = bufs
            .par_chunks(chunk)
            .map(|slice| {
                let mut tree = Vec::with_capacity(chunk * 2 - 1);
                slice
                    .iter()
                    .for_each(|b| tree.push(TurbineMerkleHash::hash(&[b.as_ref()])));
                let mut base = 0;
                let mut level_leaves = slice.len();
                while level_leaves > 1 {
                    for i in (0..level_leaves).step_by(2) {
                        let hash =
                            TurbineMerkleHash::hash(&[&tree[base + i].0, &tree[base + i + 1].0]);
                        tree.push(hash);
                    }
                    base += level_leaves;
                    level_leaves >>= 1;
                }
                tree
            })
            .collect();

        let tree_size = bufs.len() * 2 - 1;
        let mut tree = Vec::with_capacity(tree_size);

        // copy subtrees
        let mut level_leaves = chunk;
        let mut base = 0;
        while level_leaves >= 1 {
            for sub_tree in sub_trees.iter() {
                for i in 0..level_leaves {
                    tree.push(sub_tree[base + i]);
                }
            }
            base += level_leaves;
            level_leaves >>= 1;
        }

        // compute final levels of tree
        level_leaves = bufs.len() / chunk;
        base = (chunk * 2 - 1) * level_leaves - level_leaves;
        while level_leaves > 1 {
            for i in (0..level_leaves).step_by(2) {
                let hash = TurbineMerkleHash::hash(&[&tree[base + i].0, &tree[base + i + 1].0]);
                tree.push(hash);
            }
            base += level_leaves;
            level_leaves >>= 1;
        }

        Self { tree }
    }

    #[allow(clippy::integer_arithmetic)]
    pub fn new_from_bufs_vec_par<T>(bufs_vec: &Vec<Vec<T>>, chunk: usize) -> Self
    where
        T: AsRef<[u8]> + Sync,
    {
        // compute subtrees of chunk width in parallel
        let sub_trees: Vec<Vec<TurbineMerkleHash>> = bufs_vec
            .par_chunks(chunk)
            .map(|slice: &[Vec<T>]| {
                let mut tree = Vec::with_capacity(chunk * 2 - 1);
                slice
                    .iter()
                    .for_each(|v: &Vec<T>| tree.push(TurbineMerkleHash::hash(v)));
                let mut base = 0;
                let mut level_leaves = slice.len();
                while level_leaves > 1 {
                    for i in (0..level_leaves).step_by(2) {
                        let hash =
                            TurbineMerkleHash::hash(&[&tree[base + i].0, &tree[base + i + 1].0]);
                        tree.push(hash);
                    }
                    base += level_leaves;
                    level_leaves >>= 1;
                }
                tree
            })
            .collect();

        let tree_size = bufs_vec.len() * 2 - 1;
        let mut tree = Vec::with_capacity(tree_size);

        // copy subtrees
        let mut level_leaves = chunk;
        let mut base = 0;
        while level_leaves >= 1 {
            for sub_tree in sub_trees.iter() {
                for i in 0..level_leaves {
                    tree.push(sub_tree[base + i]);
                }
            }
            base += level_leaves;
            level_leaves >>= 1;
        }

        // compute final levels of tree
        level_leaves = bufs_vec.len() / chunk;
        base = (chunk * 2 - 1) * level_leaves - level_leaves;
        while level_leaves > 1 {
            for i in (0..level_leaves).step_by(2) {
                let hash = TurbineMerkleHash::hash(&[&tree[base + i].0, &tree[base + i + 1].0]);
                tree.push(hash);
            }
            base += level_leaves;
            level_leaves >>= 1;
        }

        Self { tree }
    }

    #[allow(clippy::integer_arithmetic)]
    pub fn leaf_count(&self) -> usize {
        (self.tree.len() + 1) / 2
    }

    #[allow(clippy::integer_arithmetic)]
    pub fn root(&self) -> TurbineMerkleHash {
        self.tree[self.tree.len() - 1]
    }

    pub fn node(&self, index: usize) -> TurbineMerkleHash {
        self.tree[index]
    }

    #[allow(clippy::integer_arithmetic)]
    pub fn prove(&self, leaf_index: usize) -> TurbineMerkleProof {
        let mut proof = Vec::new();
        let mut level_leaves = self.leaf_count();
        let mut i = leaf_index;
        let mut base = 0;
        while level_leaves > 1 {
            if i & 1 == 0 {
                proof.push(self.tree[base + i + 1]);
            } else {
                proof.push(self.tree[base + i - 1]);
            }
            base += level_leaves;
            i >>= 1;
            level_leaves >>= 1;
        }
        TurbineMerkleProof(proof)
    }

    pub fn prove_fec64(&self, leaf_index: usize) -> TurbineMerkleProofFec64 {
        TurbineMerkleProofFec64(self.prove(leaf_index).0.try_into().expect("6 elements"))
    }

    #[allow(clippy::integer_arithmetic)]
    fn _print(&self) {
        let mut base = 0;
        let mut level_nodes = self.leaf_count();
        while level_nodes > 0 {
            println!("{:?}", &self.tree[base..base + level_nodes]);
            base += level_nodes;
            level_nodes /= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_random_packets(npackets: usize) -> Vec<Vec<u8>> {
        let mut packets = Vec::default();
        for _i in 0..npackets {
            let buf: Vec<u8> = (0..1024).map(|_| rand::random::<u8>()).collect();
            packets.push(buf);
        }
        packets
    }

    #[test]
    fn test_merkle() {
        let packets = create_random_packets(16);
        let leaves: Vec<TurbineMerkleHash> = packets
            .iter()
            .map(|p| TurbineMerkleHash::hash(&[p]))
            .collect();

        let tree = TurbineMerkleTree::new_from_leaves(&leaves);
        let root = tree.root();
        let proof5 = tree.prove(5);

        assert!(proof5.verify(&root, &leaves[5], 5));
        assert!(proof5.verify(&root, &tree.node(5), 5));
    }

    #[test]
    fn test_merkle_from_buf_par() {
        let packets = create_random_packets(128);
        let ref_tree = TurbineMerkleTree::new_from_bufs(&packets[..]);
        let mut chunk_width = 1;
        while chunk_width <= 128 {
            let tree = TurbineMerkleTree::new_from_bufs_par(&packets[..], chunk_width);
            assert_eq!(&ref_tree, &tree);
            chunk_width *= 2;
        }
    }

    #[test]
    fn test_merkle_from_buf_vecs_par() {
        let packets = create_random_packets(64);
        let ref_tree = TurbineMerkleTree::new_from_bufs(&packets[..]);
        let bufs_vec: Vec<_> = packets.iter().map(|p| vec![&p[..20], &p[20..]]).collect();
        let mut chunk_width = 1;
        while chunk_width <= 64 {
            let tree = TurbineMerkleTree::new_from_bufs_vec_par(&bufs_vec, chunk_width);
            assert_eq!(&ref_tree, &tree);
            chunk_width *= 2;
        }
    }
}
