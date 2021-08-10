//! IBC validity predicate for channel module

use std::str::FromStr;

use borsh::BorshDeserialize;
use ibc::ics02_client::client_consensus::AnyConsensusState;
use ibc::ics02_client::client_state::AnyClientState;
use ibc::ics02_client::context::ClientReader;
use ibc::ics02_client::height::Height;
use ibc::ics03_connection::connection::ConnectionEnd;
use ibc::ics03_connection::context::ConnectionReader;
use ibc::ics04_channel::channel::{ChannelEnd, Counterparty, State};
use ibc::ics04_channel::context::ChannelReader;
use ibc::ics04_channel::error::{Error as Ics04Error, Kind as Ics04Kind};
use ibc::ics04_channel::handler::verify::verify_channel_proofs;
use ibc::ics04_channel::packet::{Receipt, Sequence};
use ibc::ics05_port::capabilities::Capability;
use ibc::ics05_port::context::PortReader;
use ibc::ics24_host::identifier::{ChannelId, ClientId, ConnectionId, PortId};
use ibc::ics24_host::Path;
use ibc::proofs::Proofs;
use ibc::timestamp::Timestamp;
use sha2::Digest;
use tendermint_proto::Protobuf;
use thiserror::Error;

use super::{Ibc, StateChange};
use crate::ledger::native_vp::Error as NativeVpError;
use crate::ledger::storage::{self, StorageHasher};
use crate::types::ibc::{
    ChannelCloseConfirmData, ChannelCloseInitData, ChannelOpenAckData,
    ChannelOpenConfirmData, ChannelOpenTryData, Error as IbcDataError,
};
use crate::types::storage::{Key, KeySeg};

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("Native VP error: {0}")]
    NativeVpError(NativeVpError),
    #[error("Key error: {0}")]
    KeyError(String),
    #[error("State change error: {0}")]
    StateChangeError(String),
    #[error("Connection error: {0}")]
    ConnectionError(String),
    #[error("Channel error: {0}")]
    ChannelError(String),
    #[error("Port error: {0}")]
    PortError(String),
    #[error("Version error: {0}")]
    VersionError(String),
    #[error("Sequence error: {0}")]
    SequenceError(String),
    #[error("Packet info error: {0}")]
    PacketInfoError(String),
    #[error("Proof verification error: {0}")]
    ProofVerificationError(Ics04Error),
    #[error("Decoding TX data error: {0}")]
    DecodingTxDataError(std::io::Error),
    #[error("IBC data error: {0}")]
    IbcDataError(IbcDataError),
}

/// IBC channel functions result
pub type Result<T> = std::result::Result<T, Error>;

impl<'a, DB, H> Ibc<'a, DB, H>
where
    DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
    H: 'static + StorageHasher,
{
    pub(super) fn validate_channel(
        &self,
        key: &Key,
        tx_data: &[u8],
    ) -> Result<bool> {
        if key.is_ibc_channel_counter() {
            return Ok(self.channel_counter_pre()? < self.channel_counter());
        }

        let port_id = Self::get_port_id(key)
            .map_err(|e| Error::KeyError(e.to_string()))?;
        let channel_id = Self::get_channel_id(key)?;

        self.authenticated_capability(&port_id).map_err(|e| {
            Error::PortError(format!(
                "The port is not authenticated: ID {}, {}",
                port_id, e
            ))
        })?;

        let port_channel_id = (port_id, channel_id);
        let channel = match self.channel_end(&port_channel_id) {
            Some(c) => c,
            None => {
                return Err(Error::ChannelError(format!(
                    "The channel doesn't exist: Port {}, Channel {}",
                    port_channel_id.0, port_channel_id.1
                )));
            }
        };
        // check the number of hops and empty version in the channel end
        channel.validate_basic().map_err(|e| {
            Error::ChannelError(format!(
                "The channel is invalid: Port {}, Channel {}, {}",
                port_channel_id.0, port_channel_id.1, e
            ))
        })?;

        self.validate_version(&channel)?;

        match self.get_channel_state_change(port_channel_id.clone())? {
            StateChange::Created => match channel.state() {
                State::Init => Ok(true),
                State::TryOpen => self.verify_channel_try_proof(
                    port_channel_id,
                    &channel,
                    tx_data,
                ),
                _ => Err(Error::ChannelError(format!(
                    "The channel state is invalid: Port {}, Channel {}, State \
                     {}",
                    port_channel_id.0,
                    port_channel_id.1,
                    channel.state()
                ))),
            },
            StateChange::Updated => self.validate_updated_channel(
                port_channel_id,
                &channel,
                tx_data,
            ),
            _ => Err(Error::StateChangeError(format!(
                "The state change of the channel: Port {}, Channel {}",
                port_channel_id.0, port_channel_id.1
            ))),
        }
    }

    /// Returns the channel ID after #IBC/channelEnds/ports/{port_id}/channels
    fn get_channel_id(key: &Key) -> Result<ChannelId> {
        match key.segments.get(5) {
            Some(id) => ChannelId::from_str(&id.raw())
                .map_err(|e| Error::KeyError(e.to_string())),
            None => Err(Error::KeyError(format!(
                "The key doesn't have a channel ID: {}",
                key
            ))),
        }
    }

    fn get_channel_state_change(
        &self,
        port_channel_id: (PortId, ChannelId),
    ) -> Result<StateChange> {
        let path =
            Path::ChannelEnds(port_channel_id.0, port_channel_id.1).to_string();
        let key =
            Key::ibc_key(path).expect("Creating a key for a channel failed");
        self.get_state_change(&key)
            .map_err(|e| Error::StateChangeError(e.to_string()))
    }

    fn validate_version(&self, channel: &ChannelEnd) -> Result<()> {
        let connection = self.connection_from_channel(channel)?;
        let versions = connection.versions();
        let version = match versions.as_slice() {
            [version] => version,
            _ => {
                return Err(Error::VersionError(
                    "Multiple versions are specified or no version".to_owned(),
                ));
            }
        };

        let feature = channel.ordering().to_string();
        if version.is_supported_feature(feature.clone()) {
            Ok(())
        } else {
            Err(Error::VersionError(format!(
                "The version is unsupported: Feature {}",
                feature
            )))
        }
    }

    fn validate_updated_channel(
        &self,
        port_channel_id: (PortId, ChannelId),
        channel: &ChannelEnd,
        tx_data: &[u8],
    ) -> Result<bool> {
        let prev_channel = self.channel_end_pre(port_channel_id.clone())?;
        match channel.state() {
            State::Open => match prev_channel.state() {
                State::Init => self.verify_channel_ack_proof(
                    port_channel_id,
                    channel,
                    tx_data,
                ),
                State::TryOpen => self.verify_channel_confirm_proof(
                    port_channel_id,
                    channel,
                    tx_data,
                ),
                _ => Err(Error::StateChangeError(format!(
                    "The state change of the channel is invalid: Port {}, \
                     Channel {}",
                    port_channel_id.0, port_channel_id.1,
                ))),
            },
            State::Closed => {
                if !prev_channel.state_matches(&State::Open) {
                    return Err(Error::StateChangeError(format!(
                        "The state change of the channel is invalid: Port {}, \
                         Channel {}",
                        port_channel_id.0, port_channel_id.1,
                    )));
                }
                match ChannelCloseInitData::try_from_slice(tx_data) {
                    Ok(_) => Ok(true),
                    Err(_) => self.verify_channel_close_proof(
                        port_channel_id,
                        channel,
                        tx_data,
                    ),
                }
            }
            _ => Err(Error::StateChangeError(format!(
                "The state change of the channel is invalid: Port {}, Channel \
                 {}",
                port_channel_id.0, port_channel_id.1
            ))),
        }
    }

    fn verify_channel_try_proof(
        &self,
        port_channel_id: (PortId, ChannelId),
        channel: &ChannelEnd,
        tx_data: &[u8],
    ) -> Result<bool> {
        let data = ChannelOpenTryData::try_from_slice(tx_data)?;
        let expected_my_side = Counterparty::new(port_channel_id.0, None);

        self.verify_proofs(
            channel,
            expected_my_side,
            State::Init,
            data.proofs()?,
        )
    }

    fn verify_channel_ack_proof(
        &self,
        port_channel_id: (PortId, ChannelId),
        channel: &ChannelEnd,
        tx_data: &[u8],
    ) -> Result<bool> {
        let data = ChannelOpenAckData::try_from_slice(tx_data)?;
        let expected_my_side =
            Counterparty::new(port_channel_id.0, Some(port_channel_id.1));

        self.verify_proofs(
            channel,
            expected_my_side,
            State::TryOpen,
            data.proofs()?,
        )
    }

    fn verify_channel_confirm_proof(
        &self,
        port_channel_id: (PortId, ChannelId),
        channel: &ChannelEnd,
        tx_data: &[u8],
    ) -> Result<bool> {
        let data = ChannelOpenConfirmData::try_from_slice(tx_data)?;
        let expected_my_side =
            Counterparty::new(port_channel_id.0, Some(port_channel_id.1));

        self.verify_proofs(
            channel,
            expected_my_side,
            State::Open,
            data.proofs()?,
        )
    }

    fn verify_channel_close_proof(
        &self,
        port_channel_id: (PortId, ChannelId),
        channel: &ChannelEnd,
        tx_data: &[u8],
    ) -> Result<bool> {
        let data = ChannelCloseConfirmData::try_from_slice(tx_data)?;
        let expected_my_side =
            Counterparty::new(port_channel_id.0, Some(port_channel_id.1));

        self.verify_proofs(
            channel,
            expected_my_side,
            State::Closed,
            data.proofs()?,
        )
    }

    fn verify_proofs(
        &self,
        channel: &ChannelEnd,
        expected_my_side: Counterparty,
        expected_state: State,
        proofs: Proofs,
    ) -> Result<bool> {
        let connection = self.connection_from_channel(channel)?;
        let counterpart_conn_id =
            match connection.counterparty().connection_id() {
                Some(id) => id.clone(),
                None => {
                    return Err(Error::ConnectionError(
                        "The counterpart connection ID doesn't exist"
                            .to_owned(),
                    ));
                }
            };
        let expected_connection_hops = vec![counterpart_conn_id];
        let expected_channel = ChannelEnd::new(
            expected_state,
            *channel.ordering(),
            expected_my_side,
            expected_connection_hops,
            channel.version(),
        );

        match verify_channel_proofs(
            self,
            &channel,
            &connection,
            &expected_channel,
            &proofs,
        ) {
            Ok(_) => Ok(true),
            Err(e) => Err(Error::ProofVerificationError(e)),
        }
    }

    fn get_sequence(&self, path: Path) -> Result<Sequence> {
        let key = Key::ibc_key(path.to_string())
            .expect("Creating akey for a sequence shouldn't fail");
        match self.ctx.read_post(&key)? {
            Some(value) => {
                let index: u64 =
                    storage::types::decode(value).map_err(|e| {
                        Error::SequenceError(format!(
                            "Decoding a sequece index failed: {}",
                            e
                        ))
                    })?;
                Ok(Sequence::from(index))
            }
            None => Err(Error::SequenceError(format!(
                "The sequence doesn't exist: Path {}",
                path
            ))),
        }
    }

    fn get_packet_info(&self, path: Path) -> Result<String> {
        let key = Key::ibc_key(path.to_string())
            .expect("Creating akey for a packet info shouldn't fail");
        match self.ctx.read_post(&key)? {
            Some(value) => String::from_utf8(value.to_vec()).map_err(|e| {
                Error::PacketInfoError(format!(
                    "Decoding the packet info failed: {}",
                    e
                ))
            }),
            None => Err(Error::PacketInfoError(format!(
                "The packet info doesn't exist: Path {}",
                path
            ))),
        }
    }

    fn connection_from_channel(
        &self,
        channel: &ChannelEnd,
    ) -> Result<ConnectionEnd> {
        match channel.connection_hops().get(0) {
            Some(conn_id) => {
                match ChannelReader::connection_end(self, &conn_id) {
                    Some(conn) => Ok(conn),
                    None => Err(Error::ConnectionError(format!(
                        "The connection doesn't exist: ID {}",
                        conn_id
                    ))),
                }
            }
            _ => Err(Error::ConnectionError(
                "the corresponding connection ID doesn't exist".to_owned(),
            )),
        }
    }

    fn channel_end_pre(
        &self,
        port_channel_id: (PortId, ChannelId),
    ) -> Result<ChannelEnd> {
        let path = Path::ChannelEnds(
            port_channel_id.0.clone(),
            port_channel_id.1.clone(),
        )
        .to_string();
        let key =
            Key::ibc_key(path).expect("Creating a key for a channel failed");
        match self.ctx.read_pre(&key) {
            Ok(Some(value)) => ChannelEnd::decode_vec(&value).map_err(|e| {
                Error::ChannelError(format!(
                    "Decoding the channel failed: Port {}, Channel {}, {}",
                    port_channel_id.0, port_channel_id.1, e
                ))
            }),
            Ok(None) => Err(Error::ChannelError(format!(
                "The prior channel doesn't exist: Port {}, Channel {}",
                port_channel_id.0, port_channel_id.1
            ))),
            Err(e) => Err(Error::ChannelError(format!(
                "Reading the prior channel failed: {}",
                e
            ))),
        }
    }

    fn channel_counter_pre(&self) -> Result<u64> {
        let key = Key::ibc_channel_counter();
        self.read_counter_pre(&key)
            .map_err(|e| Error::ChannelError(e.to_string()))
    }
}

impl<'a, DB, H> ChannelReader for Ibc<'a, DB, H>
where
    DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
    H: 'static + StorageHasher,
{
    fn channel_end(
        &self,
        port_channel_id: &(PortId, ChannelId),
    ) -> Option<ChannelEnd> {
        let port_channel_id = port_channel_id.clone();
        let path =
            Path::ChannelEnds(port_channel_id.0, port_channel_id.1).to_string();
        let key =
            Key::ibc_key(path).expect("Creating a key for a channel failed");
        match self.ctx.read_post(&key) {
            Ok(Some(value)) => ChannelEnd::decode_vec(&value).ok(),
            // returns None even if DB read fails
            _ => None,
        }
    }

    fn connection_end(&self, conn_id: &ConnectionId) -> Option<ConnectionEnd> {
        ConnectionReader::connection_end(self, conn_id)
    }

    fn connection_channels(
        &self,
        conn_id: &ConnectionId,
    ) -> Option<Vec<(PortId, ChannelId)>> {
        let mut channels = vec![];
        let prefix = Key::parse("channelEnds/ports")
            .expect("Creating a key for the prefix shouldn't fail");
        let mut iter = match self.ctx.iter_prefix(&prefix) {
            Ok(i) => i,
            Err(_) => return None,
        };
        loop {
            let next = match self.ctx.iter_post_next(&mut iter) {
                Ok(n) => n,
                Err(_) => return None,
            };
            if let Some((key, value)) = next {
                let channel = match ChannelEnd::decode_vec(&value) {
                    Ok(c) => c,
                    Err(_) => return None,
                };
                if let Some(id) = channel.connection_hops().get(0) {
                    if id == conn_id {
                        let key = match Key::parse(&key) {
                            Ok(k) => k,
                            Err(_) => return None,
                        };
                        let port_id = match Self::get_port_id(&key) {
                            Ok(id) => id,
                            Err(_) => return None,
                        };
                        let channel_id = match Self::get_channel_id(&key) {
                            Ok(id) => id,
                            Err(_) => return None,
                        };
                        channels.push((port_id, channel_id));
                    }
                }
            } else {
                break;
            }
        }
        Some(channels)
    }

    fn client_state(&self, client_id: &ClientId) -> Option<AnyClientState> {
        ClientReader::client_state(self, client_id)
    }

    fn client_consensus_state(
        &self,
        client_id: &ClientId,
        height: Height,
    ) -> Option<AnyConsensusState> {
        ClientReader::consensus_state(self, client_id, height)
    }

    fn authenticated_capability(
        &self,
        port_id: &PortId,
    ) -> std::result::Result<Capability, Ics04Error> {
        match self.lookup_module_by_port(port_id) {
            Some(cap) => {
                if self.authenticate(&cap, port_id) {
                    Ok(cap)
                } else {
                    Err(Ics04Kind::InvalidPortCapability.into())
                }
            }
            None => Err(Ics04Kind::NoPortCapability(port_id.clone()).into()),
        }
    }

    fn get_next_sequence_send(
        &self,
        port_channel_id: &(PortId, ChannelId),
    ) -> Option<Sequence> {
        let port_channel_id = port_channel_id.clone();
        let path = Path::SeqSends(port_channel_id.0, port_channel_id.1);
        self.get_sequence(path).ok()
    }

    fn get_next_sequence_recv(
        &self,
        port_channel_id: &(PortId, ChannelId),
    ) -> Option<Sequence> {
        let port_channel_id = port_channel_id.clone();
        let path = Path::SeqRecvs(port_channel_id.0, port_channel_id.1);
        self.get_sequence(path).ok()
    }

    fn get_next_sequence_ack(
        &self,
        port_channel_id: &(PortId, ChannelId),
    ) -> Option<Sequence> {
        let port_channel_id = port_channel_id.clone();
        let path = Path::SeqAcks(port_channel_id.0, port_channel_id.1);
        self.get_sequence(path).ok()
    }

    fn get_packet_commitment(
        &self,
        key: &(PortId, ChannelId, Sequence),
    ) -> Option<String> {
        let key = key.clone();
        let path = Path::Commitments {
            port_id: key.0,
            channel_id: key.1,
            sequence: key.2,
        };
        self.get_packet_info(path).ok()
    }

    fn get_packet_receipt(
        &self,
        key: &(PortId, ChannelId, Sequence),
    ) -> Option<Receipt> {
        let key = key.clone();
        let path = Path::Receipts {
            port_id: key.0,
            channel_id: key.1,
            sequence: key.2,
        };
        let key = Key::ibc_key(path.to_string())
            .expect("Creating akey for a packet info shouldn't fail");
        match self.ctx.read_post(&key) {
            Ok(Some(_)) => Some(Receipt::Ok),
            // returns None even if DB read fails
            _ => None,
        }
    }

    fn get_packet_acknowledgement(
        &self,
        key: &(PortId, ChannelId, Sequence),
    ) -> Option<String> {
        let path = Path::Acks {
            port_id: key.0.clone(),
            channel_id: key.1.clone(),
            sequence: key.2,
        };
        self.get_packet_info(path).ok()
    }

    fn hash(&self, value: String) -> String {
        let r = sha2::Sha256::digest(value.as_bytes());
        format!("{:x}", r)
    }

    fn host_height(&self) -> Height {
        self.host_current_height()
    }

    fn host_timestamp(&self) -> Timestamp {
        match self.ctx.storage.get_block_header().0 {
            Some(h) => Timestamp::from_datetime(h.time.into()),
            None => Timestamp::none(),
        }
    }

    fn channel_counter(&self) -> u64 {
        let key = Key::ibc_channel_counter();
        self.read_counter(&key)
    }
}

impl From<NativeVpError> for Error {
    fn from(err: NativeVpError) -> Self {
        Self::NativeVpError(err)
    }
}

impl From<IbcDataError> for Error {
    fn from(err: IbcDataError) -> Self {
        Self::IbcDataError(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::DecodingTxDataError(err)
    }
}
