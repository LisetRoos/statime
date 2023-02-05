use crate::datastructures::common::{PortIdentity, Timestamp};
use crate::datastructures::messages::{
    DelayRespMessage, FollowUpMessage, Message, MessageBuilder, SyncMessage,
};
use crate::network::NetworkPort;
use crate::port::sequence_id::SequenceIdGenerator;
use crate::port::Measurement;
use crate::time::{Duration, Instant};

type Result<T, E = SlaveError> = core::result::Result<T, E>;

#[derive(Debug)]
pub struct SlaveState {
    pub(crate) remote_master: PortIdentity,

    handshake: Handshake,
    delay_handshake: DelayHandshake,

    delay_req_ids: SequenceIdGenerator,

    next_delay_measurement: Option<Instant>,
    pending_followup: Option<FollowUpMessage>,
}

#[derive(Debug)]
enum Handshake {
    Initial,
    AfterSync {
        sync_id: u16,
        sync_recv_time: Instant,
        sync_correction: Duration,
    },
    AfterFollowUp {
        sync_id: u16,
        sync_recv_time: Instant,
        sync_send_time: Instant,
    },
    AfterDelayResp {
        sync_id: u16,
        sync_recv_time: Instant,
        sync_send_time: Instant,
        delay_recv_time: Instant,
    },
}

#[derive(Debug)]
enum DelayHandshake {
    Initial,
    AfterSync { delay_id: u16, delay_send_id: usize },
    AfterTimestamp { delay_send_time: Instant },
    AfterMeasurement { mean_delay: Duration },
}

impl DelayHandshake {
    pub fn finished(&self) -> bool {
        match self {
            DelayHandshake::Initial
            | DelayHandshake::AfterSync { .. }
            | DelayHandshake::AfterTimestamp { .. } => false,
            DelayHandshake::AfterMeasurement { .. } => true,
        }
    }
}

impl SlaveState {
    pub fn new(remote_master: PortIdentity) -> Self {
        SlaveState {
            remote_master,
            handshake: Handshake::Initial,
            delay_handshake: DelayHandshake::Initial,
            delay_req_ids: SequenceIdGenerator::new(),
            next_delay_measurement: None,
            pending_followup: None,
        }
    }

    pub(crate) fn handle_message<P: NetworkPort>(
        &mut self,
        message: Message,
        current_time: Instant,
        tc_port: &mut P,
        port_identity: PortIdentity,
    ) -> Result<()> {
        // Only listen to master
        if message.header().source_port_identity() != self.remote_master {
            match message {
                Message::Sync(message) => {
                    self.handle_sync(message, current_time, tc_port, port_identity)
                }
                Message::FollowUp(message) => self.handle_follow_up(message),
                Message::DelayResp(message) => self.handle_delay_resp(message),
                _ => Err(SlaveError::UnexpectedMessage),
            }
        } else {
            Ok(())
        }
    }

    fn handle_sync<P: NetworkPort>(
        &mut self,
        message: SyncMessage,
        current_time: Instant,
        tc_port: &mut P,
        port_identity: PortIdentity,
    ) -> Result<()> {
        match self.handshake {
            Handshake::Initial => {
                self.handshake = if message.header().two_step_flag() {
                    Handshake::AfterSync {
                        sync_id: message.header().sequence_id(),
                        sync_recv_time: current_time,
                        sync_correction: Duration::from(message.header().correction_field()),
                    }
                } else {
                    Handshake::AfterFollowUp {
                        sync_id: message.header().sequence_id(),
                        sync_recv_time: current_time,
                        sync_send_time: Instant::from(message.origin_timestamp())
                            + Duration::from(message.header().correction_field()),
                    }
                };

                if !self.delay_handshake.finished()
                    || self.next_delay_measurement.unwrap_or_default() < current_time
                {
                    let delay_id = self.delay_req_ids.generate();
                    let delay_req = MessageBuilder::new()
                        .source_port_identity(port_identity)
                        .sequence_id(delay_id)
                        .log_message_interval(0x7F)
                        .delay_req_message(Timestamp::default());
                    let delay_req_encode = delay_req.serialize_vec().unwrap();
                    let delay_send_id = tc_port
                        .send(&delay_req_encode)
                        .expect("Program error: missing timestamp id");
                    self.delay_handshake = DelayHandshake::AfterSync {
                        delay_id,
                        delay_send_id,
                    };
                } else {
                    // TODO: Seems very weird to me
                    self.delay_handshake = DelayHandshake::Initial;
                }

                if let Some(follow_up) = self.pending_followup {
                    self.handle_follow_up(follow_up);
                }

                Ok(())
            }
            // Wrong state
            Handshake::AfterSync { .. }
            | Handshake::AfterFollowUp { .. }
            | Handshake::AfterDelayResp { .. } => Err(SlaveError::OutOfSequence),
        }
    }

    fn handle_follow_up(&mut self, message: FollowUpMessage) -> Result<()> {
        match self.handshake {
            Handshake::AfterSync {
                sync_id,
                sync_recv_time,
                sync_correction,
            } => {
                if sync_id == message.header().sequence_id() {
                    // Remove any previous pending messages, they are no longer current
                    self.pending_followup = None;

                    // Absorb into state
                    let sync_send_time = Instant::from(message.precise_origin_timestamp())
                        + Duration::from(message.header().correction_field())
                        + sync_correction;
                    self.handshake = Handshake::AfterFollowUp {
                        sync_id,
                        sync_recv_time,
                        sync_send_time,
                    };

                    Ok(())
                } else {
                    // Ignore messages not belonging to currently processing sync
                    self.pending_followup = Some(message); // Store it for a potentially coming sync
                    Ok(())
                }
            }
            // Wrong state
            Handshake::Initial
            | Handshake::AfterFollowUp { .. }
            | Handshake::AfterDelayResp { .. } => Err(SlaveError::OutOfSequence),
        }
    }

    fn handle_delay_resp(&mut self, message: DelayRespMessage) -> Result<()> {
        match self.handshake {
            Handshake::AfterFollowUp {
                sync_id,
                sync_recv_time,
                sync_send_time,
            } => {
                match self.delay_handshake {
                    DelayHandshake::AfterSync { delay_id, .. } => {
                        // Ignore messages not belonging to currently processing sync
                        if delay_id != message.header().sequence_id() {
                            return Ok(());
                        }

                        // Absorb into state
                        let delay_recv_time = Instant::from(message.receive_timestamp())
                            - Duration::from(message.header().correction_field());
                        self.handshake = Handshake::AfterDelayResp {
                            sync_id,
                            sync_recv_time,
                            sync_send_time,
                            delay_recv_time,
                        };

                        // Calculate when we should next measure delay
                        //  note that sync_recv_time should always be set here, but if it isn't,
                        //  taking the default (0) is safe for recovery.
                        self.next_delay_measurement = Some(
                            sync_recv_time
                                + Duration::from_log_interval(
                                    message.header().log_message_interval(),
                                )
                                - Duration::from_fixed_nanos(0.1f64),
                        );

                        self.finish_delay_measurement();

                        Ok(())
                    }
                    // Wrong state
                    DelayHandshake::Initial
                    | DelayHandshake::AfterTimestamp { .. }
                    | DelayHandshake::AfterMeasurement { .. } => Err(SlaveError::OutOfSequence),
                }
            }
            // Wrong state
            Handshake::Initial | Handshake::AfterSync { .. } | Handshake::AfterDelayResp { .. } => {
                Err(SlaveError::OutOfSequence)
            }
        }
    }

    pub(crate) fn handle_send_timestamp(&mut self, id: usize, timestamp: Instant) -> Result<()> {
        match self.delay_handshake {
            DelayHandshake::AfterSync { delay_send_id, .. } => {
                if delay_send_id == id {
                    self.delay_handshake = DelayHandshake::AfterTimestamp {
                        delay_send_time: timestamp,
                    };
                    self.finish_delay_measurement()
                } else {
                    Err(SlaveError::OutOfSequence)
                }
            }
            // Wrong state
            DelayHandshake::Initial
            | DelayHandshake::AfterTimestamp { .. }
            | DelayHandshake::AfterMeasurement { .. } => Err(SlaveError::OutOfSequence),
        }
    }

    fn finish_delay_measurement(&mut self) -> Result<()> {
        match self.handshake {
            Handshake::AfterDelayResp {
                sync_id,
                sync_recv_time,
                sync_send_time,
                delay_recv_time,
            } => {
                match self.delay_handshake {
                    DelayHandshake::AfterTimestamp { delay_send_time } => {
                        let mean_delay = (sync_recv_time - sync_send_time
                            + (delay_recv_time - delay_send_time))
                            / 2;

                        self.delay_handshake = DelayHandshake::AfterMeasurement { mean_delay };
                        self.handshake = Handshake::AfterFollowUp {
                            sync_id,
                            sync_recv_time,
                            sync_send_time,
                        };

                        Ok(())
                    }
                    // Wrong state
                    DelayHandshake::Initial
                    | DelayHandshake::AfterSync { .. }
                    | DelayHandshake::AfterMeasurement { .. } => Err(SlaveError::OutOfSequence),
                }
            }
            // Wrong state
            Handshake::Initial | Handshake::AfterSync { .. } | Handshake::AfterFollowUp { .. } => {
                Err(SlaveError::OutOfSequence)
            }
        }
    }

    pub(crate) fn extract_measurement(&mut self) -> Result<Measurement> {
        match self.handshake {
            Handshake::AfterFollowUp {
                sync_recv_time,
                sync_send_time,
                ..
            }
            | Handshake::AfterDelayResp {
                sync_recv_time,
                sync_send_time,
                ..
            } => {
                match self.delay_handshake {
                    DelayHandshake::AfterMeasurement { mean_delay } => {
                        let result = Measurement {
                            master_offset: sync_recv_time - sync_send_time - mean_delay,
                            event_time: sync_recv_time,
                        };

                        self.handshake = Handshake::Initial;

                        Ok(result)
                    }
                    // Wrong state
                    DelayHandshake::Initial
                    | DelayHandshake::AfterSync { .. }
                    | DelayHandshake::AfterTimestamp { .. } => Err(SlaveError::OutOfSequence),
                }
            }
            // Wrong state
            Handshake::Initial | Handshake::AfterSync { .. } => Err(SlaveError::OutOfSequence),
        }
    }
}

#[derive(Debug)]
pub enum SlaveError {
    /// Received a message that a port in the slave state can never process.
    UnexpectedMessage,
    /// Received a message that can usually be processed, but not right now.
    OutOfSequence,
}
