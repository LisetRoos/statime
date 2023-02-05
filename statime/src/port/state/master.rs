use crate::datastructures::common::{PortIdentity, Timestamp};
use crate::datastructures::messages::{DelayReqMessage, Message, MessageBuilder};
use crate::network::NetworkPort;
use crate::port::error::{PortError, Result};
use crate::port::sequence_id::SequenceIdGenerator;
use crate::time::Instant;

#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MasterState {
    pub(crate) announce_seq_ids: SequenceIdGenerator,
    pub(crate) sync_seq_ids: SequenceIdGenerator,
    pub(crate) follow_up_seq_ids: SequenceIdGenerator,
    pub(crate) delay_resp_seq_ids: SequenceIdGenerator,
}

impl MasterState {
    pub fn new() -> Self {
        MasterState {
            announce_seq_ids: SequenceIdGenerator::new(),
            sync_seq_ids: SequenceIdGenerator::new(),
            follow_up_seq_ids: SequenceIdGenerator::new(),
            delay_resp_seq_ids: SequenceIdGenerator::new(),
        }
    }

    pub fn handle_message<P: NetworkPort>(
        &mut self,
        message: Message,
        current_time: Instant,
        nc_port: &mut P,
        port_identity: PortIdentity,
    ) -> Result<()> {
        // Always ignore messages from own port
        if message.header().source_port_identity() != port_identity {
            match message {
                Message::DelayReq(message) => {
                    self.handle_delay_req(message, current_time, nc_port, port_identity)
                }
                _ => Err(PortError::UnexpectedMessage),
            }
        } else {
            Ok(())
        }
    }

    fn handle_delay_req<P: NetworkPort>(
        &mut self,
        message: DelayReqMessage,
        current_time: Instant,
        nc_port: &mut P,
        port_identity: PortIdentity,
    ) -> Result<()> {
        let delay_resp_message = MessageBuilder::new()
            .sequence_id(self.delay_resp_seq_ids.generate())
            .source_port_identity(port_identity)
            .delay_resp_message(
                Timestamp::from(current_time),
                message.header().source_port_identity(),
            );

        let delay_resp_encode = delay_resp_message.serialize_vec()?;
        // TODO: Handle error
        nc_port.send(&delay_resp_encode);

        Ok(())
    }
}
