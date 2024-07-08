pub mod location_details;
pub mod node_identity;

use async_std::sync::RwLock;
use bitvec::vec::BitVec;
use circular_buffer::CircularBuffer;
use futures::{
    channel::mpsc::{SendError, Sender},
    SinkExt, Stream, StreamExt,
};
use hotshot_query_service::{
    availability::QueryableHeader,
    explorer::{BlockDetail, ExplorerHeader, Timestamp},
    Leaf, Resolvable,
};
use hotshot_stake_table::vec_based::StakeTable;
use hotshot_types::{
    light_client::{CircuitField, StateVerKey},
    signature_key::BLSPubKey,
    traits::{
        block_contents::BlockHeader,
        stake_table::{SnapshotVersion, StakeTableScheme},
        BlockPayload,
    },
};
pub use location_details::LocationDetails;
pub use node_identity::NodeIdentity;
use sequencer::{Header, Payload, SeqTypes};
use std::{collections::HashSet, iter::zip, sync::Arc};
use time::OffsetDateTime;

/// MAX_HISTORY represents the last N records that are stored within the
/// DataState structure for the various different sample types.
const MAX_HISTORY: usize = 50;

/// [DataState] represents the state of the data that is being stored within
/// the service.
#[cfg_attr(test, derive(Default))]
pub struct DataState {
    latest_blocks: CircularBuffer<MAX_HISTORY, BlockDetail<SeqTypes>>,
    latest_voters: CircularBuffer<MAX_HISTORY, BitVec>,
    stake_table: StakeTable<BLSPubKey, StateVerKey, CircuitField>,
    // Do we need any other data at the moment?
    node_identity: Vec<(BLSPubKey, NodeIdentity)>,
}

impl DataState {
    pub fn new(
        latest_blocks: CircularBuffer<MAX_HISTORY, BlockDetail<SeqTypes>>,
        latest_voters: CircularBuffer<MAX_HISTORY, BitVec>,
        stake_table: StakeTable<BLSPubKey, StateVerKey, CircuitField>,
        node_identity: Vec<(BLSPubKey, NodeIdentity)>,
    ) -> Self {
        Self {
            latest_blocks,
            latest_voters,
            stake_table,
            node_identity,
        }
    }

    pub fn latest_blocks(&self) -> impl Iterator<Item = &BlockDetail<SeqTypes>> {
        self.latest_blocks.iter()
    }

    pub fn latest_voters(&self) -> impl Iterator<Item = &BitVec> {
        self.latest_voters.iter()
    }

    pub fn stake_table(&self) -> &StakeTable<BLSPubKey, StateVerKey, CircuitField> {
        &self.stake_table
    }

    pub fn node_identity(&self) -> impl Iterator<Item = &(BLSPubKey, NodeIdentity)> {
        self.node_identity.iter()
    }

    pub fn replace_stake_table(
        &mut self,
        stake_table: StakeTable<BLSPubKey, StateVerKey, CircuitField>,
    ) {
        self.stake_table = stake_table;
    }

    pub fn add_latest_block(&mut self, block: BlockDetail<SeqTypes>) {
        self.latest_blocks.push_back(block);
    }

    pub fn add_latest_voters(&mut self, voters: BitVec) {
        self.latest_voters.push_back(voters);
    }

    pub fn add_node_identity(&mut self, identity: NodeIdentity) {
        self.node_identity.push((*identity.public_key(), identity));
    }
}

/// [create_block_detail_from_leaf] is a helper function that will build a
/// [BlockDetail] from the reference to [Leaf].
pub fn create_block_detail_from_leaf(leaf: &Leaf<SeqTypes>) -> BlockDetail<SeqTypes> {
    let block_header = leaf.block_header();
    let block_payload = leaf.block_payload().unwrap_or(Payload::empty().0);

    BlockDetail::<SeqTypes> {
        hash: block_header.commitment(),
        height: block_header.height,
        time: Timestamp(
            OffsetDateTime::from_unix_timestamp(block_header.timestamp as i64)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
        ),
        proposer_id: block_header.proposer_id(),
        num_transactions: block_payload.num_transactions(block_header.metadata()) as u64,
        block_reward: vec![block_header.fee_info_balance().into()],
        fee_recipient: block_header.fee_info_account(),
        size: block_payload
            .transactions(block_header.metadata())
            .fold(0, |acc, tx| acc + tx.payload().len() as u64),
    }
}

/// [ProcessLeafError] represents the error that can occur when processing
/// a [Leaf].
#[derive(Debug)]
pub enum ProcessLeafError {
    SendError(SendError),
}

impl std::fmt::Display for ProcessLeafError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessLeafError::SendError(err) => {
                write!(f, "error sending block detail to sender: {}", err)
            }
        }
    }
}

impl std::error::Error for ProcessLeafError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProcessLeafError::SendError(err) => Some(err),
        }
    }
}

/// [process_incoming_leaf] is a helper function that will process an incoming
/// [Leaf] and update the [DataState] with the new information.
/// Additionally, the block that is contained within the [Leaf] will be
/// computed into a [BlockDetail] and sent to the [Sender] so that it can be
/// processed for real-time considerations.
async fn process_incoming_leaf(
    leaf: Leaf<SeqTypes>,
    data_state: Arc<RwLock<DataState>>,
    mut block_sender: Sender<BlockDetail<SeqTypes>>,
    mut voters_sender: Sender<BitVec>,
) -> Result<(), ProcessLeafError>
where
    Header: BlockHeader<SeqTypes> + QueryableHeader<SeqTypes> + ExplorerHeader<SeqTypes>,
    Payload: BlockPayload<SeqTypes>,
{
    let block_detail = create_block_detail_from_leaf(&leaf);
    let block_detail_copy = create_block_detail_from_leaf(&leaf);

    let certificate = leaf.justify_qc();
    let signatures = &certificate.signatures;

    // Let's take a look at the quorum certificate signatures.
    // It looks like all of these blocks are being decided by the
    // same Quorum Certificate.

    // Where's the stake table?
    let signatures = signatures.as_ref();

    // Let's determine the the participants of the voter participants
    // in the Quorum Certificate.

    // We shouldn't ever have a BitVec that is empty, with the possible
    // exception of the genesis block.
    let stake_table_voters_bit_vec = signatures.map_or(Default::default(), |sig| sig.1.clone());

    // This BitVec should be in the same order as the Stake Table.
    // The StakeTable will be able to change its order between epochs,
    // which means that its order can change between blocks.
    // However, the BitVec is a really nice size in order for storing
    // information.  We should be able to remap the BitVec order from
    // the StakeTable order to our installed order representation.  This
    // should allow us to still store as a BitVec while containing our
    // out order of the voters.
    // We will need to recompute these BitVecs if the node information that
    // is stored shrinks instead of growing.

    let mut data_state_write_lock_guard = data_state.write().await;

    let stake_table = &data_state_write_lock_guard.stake_table;
    let stable_table_entries_vec = stake_table
        .try_iter(SnapshotVersion::LastEpochStart)
        .map_or(vec![], |into_iter| into_iter.collect::<Vec<_>>());

    // We have a BitVec of voters who signed the QC.
    // We can use this to determine the weight of the QC
    let stake_table_entry_voter_participation_and_entries_pairs =
        zip(stake_table_voters_bit_vec, stable_table_entries_vec);
    let stake_table_keys_that_voted = stake_table_entry_voter_participation_and_entries_pairs
        .filter(|(bit_ref, _)| *bit_ref)
        .map(|(_, entry)| {
            // Alright this is our entry that we care about.
            // In this case, we just want to determine who voted for this
            // Leaf.

            let (key, _, _): (BLSPubKey, _, _) = entry;
            key
        });

    let voters_set: HashSet<BLSPubKey> = stake_table_keys_that_voted.collect();

    let voters_bitvec = data_state_write_lock_guard.node_identity.iter().fold(
        BitVec::with_capacity(data_state_write_lock_guard.node_identity.len()),
        |mut acc, key| {
            acc.push(voters_set.contains(&key.0));
            acc
        },
    );

    data_state_write_lock_guard
        .latest_blocks
        .push_back(block_detail);
    data_state_write_lock_guard
        .latest_voters
        .push_back(voters_bitvec.clone());

    drop(data_state_write_lock_guard);

    if let Err(err) = block_sender.send(block_detail_copy).await {
        // We have an error that prevents us from continuing
        return Err(ProcessLeafError::SendError(err));
    }

    if let Err(err) = voters_sender.send(voters_bitvec).await {
        // We have an error that prevents us from continuing
        return Err(ProcessLeafError::SendError(err));
    }

    Ok(())
}

/// [process_leaf_stream] allows for the consumption of a [Stream] when
/// attempting to process new incoming [Leaf]s.
pub async fn process_leaf_stream<S>(
    mut stream: S,
    data_state: Arc<RwLock<DataState>>,
    block_sender: Sender<BlockDetail<SeqTypes>>,
    voters_senders: Sender<BitVec>,
) where
    S: Stream<Item = Leaf<SeqTypes>> + Unpin,
    Header: BlockHeader<SeqTypes> + QueryableHeader<SeqTypes> + ExplorerHeader<SeqTypes>,
    Payload: BlockPayload<SeqTypes>,
{
    loop {
        let leaf_result = stream.next().await;
        let leaf = if let Some(leaf) = leaf_result {
            leaf
        } else {
            // We have reached the end of the stream
            tracing::info!("process leaf stream: end of stream reached for leaf stream.");
            return;
        };

        if let Err(err) = process_incoming_leaf(
            leaf,
            data_state.clone(),
            block_sender.clone(),
            voters_senders.clone(),
        )
        .await
        {
            // We have an error that prevents us from continuing
            tracing::info!("process leaf stream: error processing leaf: {}", err);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{process_leaf_stream, DataState};
    use async_std::{prelude::FutureExt, sync::RwLock};
    use futures::{channel::mpsc, SinkExt, StreamExt};
    use sequencer::{
        state::{BlockMerkleTree, FeeMerkleTree},
        ChainConfig, Leaf, NodeState, ValidatedState,
    };
    use std::{sync::Arc, time::Duration};

    #[async_std::test]
    async fn test_process_leaf_error_debug() {
        let (mut sender, receiver) = mpsc::channel(1);
        // deliberately close the receiver.
        drop(receiver);

        // Attempt to receive, and we should get an error.
        let receive_result = sender.send(1).await;

        assert!(receive_result.is_err());
        let err = receive_result.unwrap_err();

        let process_leaf_err = super::ProcessLeafError::SendError(err);

        assert_eq!(
            format!("{:?}", process_leaf_err),
            "SendError(SendError { kind: Disconnected })"
        );
    }

    #[async_std::test]
    async fn test_process_leaf_stream() {
        let data_state: DataState = Default::default();
        let data_state = Arc::new(RwLock::new(data_state));
        let (block_sender, block_receiver) = futures::channel::mpsc::channel(1);
        let (voters_sender, voters_receiver) = futures::channel::mpsc::channel(1);
        let (leaf_sender, leaf_receiver) = futures::channel::mpsc::channel(1);

        let process_leaf_stream_task_handle = async_std::task::spawn(process_leaf_stream(
            leaf_receiver,
            data_state.clone(),
            block_sender,
            voters_sender,
        ));

        {
            let data_state = data_state.read().await;
            // Latest blocks should be empty
            assert_eq!(data_state.latest_blocks().count(), 0);
            // Latest voters should be empty
            assert_eq!(data_state.latest_voters().count(), 0);
        }

        let validated_state = ValidatedState {
            block_merkle_tree: BlockMerkleTree::new(32),
            fee_merkle_tree: FeeMerkleTree::new(32),
            chain_config: ChainConfig::default().into(),
        };
        let instance_state = NodeState::mock();

        let sample_leaf = Leaf::genesis(&validated_state, &instance_state).await;

        let mut leaf_sender = leaf_sender;
        // We should be able to send a leaf without issue
        assert_eq!(leaf_sender.send(sample_leaf).await, Ok(()),);

        let mut block_receiver = block_receiver;
        // We should receive a Block Detail.

        let next_block = block_receiver.next().await;
        assert!(next_block.is_some());

        let mut voters_receiver = voters_receiver;
        // We should receive a BitVec of voters.
        let next_voters = voters_receiver.next().await;
        assert!(next_voters.is_some());

        {
            let data_state = data_state.read().await;
            // Latest blocks should now have a single entry
            assert_eq!(data_state.latest_blocks().count(), 1);
            // Latest voters should now have a single entry
            assert_eq!(data_state.latest_voters().count(), 1);
        }

        // We explicitly drop these, as it should make the task clean up.
        drop(block_receiver);
        drop(leaf_sender);

        assert_eq!(
            process_leaf_stream_task_handle
                .timeout(Duration::from_millis(200))
                .await,
            Ok(())
        );
    }
}
