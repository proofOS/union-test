use std::fmt::Debug;

use cosmwasm_std::{
    attr, Addr, Binary, CosmosMsg, Event, IbcBasicResponse, IbcEndpoint, IbcMsg, IbcOrder,
    IbcReceiveResponse, Response, SubMsg, Timestamp,
};
use thiserror::Error;

use crate::types::{
    EncodingError, GenericAck, TransferPacket, TransferPacketCommon, TransferToken,
};

// https://github.com/cosmos/ibc-go/blob/8218aeeef79d556852ec62a773f2bc1a013529d4/modules/apps/transfer/types/keys.go#L12
pub const MODULE_NAME: &'static str = "transfer";

// https://github.com/cosmos/ibc-go/blob/8218aeeef79d556852ec62a773f2bc1a013529d4/modules/apps/transfer/types/events.go#L4-L22
pub const PACKET_EVENT: &'static str = "fungible_token_packet";
pub const TRANSFER_EVENT: &'static str = "ibc_transfer";
pub const TIMEOUT_EVENT: &'static str = "timeout";

#[derive(Error, Debug, PartialEq)]
pub enum ProtocolError {
    #[error("Channel doesn't exist: {channel_id}")]
    NoSuchChannel { channel_id: String },
    #[error("Protocol must be caller")]
    Unauthorized,
}

#[allow(type_alias_bounds)]
pub type PacketExtensionOf<T: TransferProtocol> = <T::Packet as TransferPacket>::Extension;

pub struct TransferInput {
    pub current_time: Timestamp,
    pub timeout_delta: u64,
    pub sender: Addr,
    pub receiver: String,
    pub tokens: Vec<TransferToken>,
}

// We follow the following module implementation, events and attributes are
// almost 1:1 with the traditional go implementation. As we generalized the base
// implementation for multi-tokens transfer, the events are not containing a
// single ('denom', 'value') and ('amount', 'value') attributes but rather a set
// of ('denom:x', 'amount_value') attributes for each denom `x` that is
// transferred. i.e. [('denom:muno', '10'), ('denom:port/channel/weth', '150'), ..]
// https://github.com/cosmos/ibc-go/blob/7be17857b10457c67cbf66a49e13a9751eb10e8e/modules/apps/transfer/ibc_module.go
pub trait TransferProtocol {
    /// Must be unique per Protocol
    const VERSION: &'static str;
    const ORDERING: IbcOrder;
    /// Must be unique per Protocol
    const RECEIVE_REPLY_ID: u64;

    type Packet: TryFrom<Binary, Error = EncodingError>
        + TryInto<Binary, Error = EncodingError>
        + TransferPacket;

    type Ack: TryFrom<Binary, Error = EncodingError>
        + TryInto<Binary, Error = EncodingError>
        + Into<GenericAck>;

    type CustomMsg;

    type Error: Debug + From<ProtocolError> + From<EncodingError>;

    fn channel_endpoint(&self) -> &IbcEndpoint;

    fn caller(&self) -> &Addr;

    fn self_addr(&self) -> &Addr;

    fn ack_success() -> Self::Ack;

    fn ack_failure(error: String) -> Self::Ack;

    fn send_tokens(
        &mut self,
        sender: &str,
        receiver: &str,
        tokens: Vec<TransferToken>,
    ) -> Result<Vec<CosmosMsg<Self::CustomMsg>>, Self::Error>;

    fn send_tokens_success(
        &mut self,
        sender: &str,
        receiver: &str,
        tokens: Vec<TransferToken>,
    ) -> Result<Vec<CosmosMsg<Self::CustomMsg>>, Self::Error>;

    fn send_tokens_failure(
        &mut self,
        sender: &str,
        receiver: &str,
        tokens: Vec<TransferToken>,
    ) -> Result<Vec<CosmosMsg<Self::CustomMsg>>, Self::Error>;

    fn send(
        &mut self,
        mut input: TransferInput,
        extension: PacketExtensionOf<Self>,
    ) -> Result<Response<Self::CustomMsg>, Self::Error> {
        input.tokens = input
            .tokens
            .into_iter()
            .map(|token| {
                token.normalize_for_ibc_transfer(self.self_addr().as_str(), self.channel_endpoint())
            })
            .collect();

        let packet = Self::Packet::try_from(TransferPacketCommon {
            sender: input.sender.to_string(),
            receiver: input.receiver.clone(),
            tokens: input.tokens.clone(),
            extension: extension.clone(),
        })?;

        let send_msgs = self.send_tokens(packet.sender(), packet.receiver(), packet.tokens())?;

        Ok(Response::new()
            .add_messages(send_msgs)
            .add_message(IbcMsg::SendPacket {
                channel_id: self.channel_endpoint().channel_id.clone(),
                data: packet.try_into()?,
                timeout: input.current_time.plus_seconds(input.timeout_delta).into(),
            })
            .add_events([
                Event::new(TRANSFER_EVENT)
                    .add_attributes([
                        ("sender", input.sender.as_str()),
                        ("receiver", input.receiver.as_str()),
                        ("memo", extension.into().as_str()),
                    ])
                    .add_attributes(input.tokens.into_iter().map(
                        |TransferToken { denom, amount }| (format!("denom:{}", denom), amount),
                    )),
                Event::new("message").add_attribute("module", MODULE_NAME),
            ]))
    }

    fn send_ack(
        &mut self,
        raw_ack: impl Into<Binary> + Clone,
        raw_packet: impl Into<Binary>,
    ) -> Result<IbcBasicResponse<Self::CustomMsg>, Self::Error> {
        let packet = Self::Packet::try_from(raw_packet.into())?;
        // https://github.com/cosmos/ibc-go/blob/5ca37ef6e56a98683cf2b3b1570619dc9b322977/modules/apps/transfer/ibc_module.go#L261
        let ack = Into::<GenericAck>::into(Self::Ack::try_from(raw_ack.clone().into())?);
        let (ack_msgs, ack_attr) = match ack {
            Ok(value) => (
                self.send_tokens_success(packet.sender(), packet.receiver(), packet.tokens())?,
                attr("success", value.to_string()),
            ),
            Err(error) => (
                self.send_tokens_failure(packet.sender(), packet.receiver(), packet.tokens())?,
                attr("error", error.to_string()),
            ),
        };
        Ok(IbcBasicResponse::new()
            .add_event(
                Event::new(PACKET_EVENT)
                    .add_attributes([
                        ("module", MODULE_NAME),
                        ("sender", packet.sender()),
                        ("receiver", packet.receiver()),
                        ("memo", packet.extension().clone().into().as_str()),
                        ("acknowledgement", &raw_ack.into().to_string()),
                    ])
                    .add_attributes(packet.tokens().into_iter().map(
                        |TransferToken { denom, amount }| (format!("denom:{}", denom), amount),
                    )),
            )
            .add_event(Event::new(PACKET_EVENT).add_attributes([ack_attr]))
            .add_messages(ack_msgs))
    }

    fn send_timeout(
        &mut self,
        raw_packet: impl Into<Binary>,
    ) -> Result<IbcBasicResponse<Self::CustomMsg>, Self::Error> {
        let packet = Self::Packet::try_from(raw_packet.into())?;
        // same branch as failure ack
        let refund_msgs =
            self.send_tokens_failure(packet.sender(), packet.receiver(), packet.tokens())?;
        Ok(IbcBasicResponse::new()
            .add_event(
                Event::new(TIMEOUT_EVENT)
                    .add_attributes([
                        ("module", MODULE_NAME),
                        ("refund_receiver", packet.sender()),
                        ("memo", packet.extension().clone().into().as_str()),
                    ])
                    .add_attributes(packet.tokens().into_iter().map(
                        |TransferToken { denom, amount }| (format!("denom:{}", denom), amount),
                    )),
            )
            .add_messages(refund_msgs))
    }

    fn make_receive_phase1_execute(
        &mut self,
        raw_packet: impl Into<Binary>,
    ) -> Result<CosmosMsg<Self::CustomMsg>, Self::Error>;

    fn receive_phase0(
        &mut self,
        raw_packet: impl Into<Binary> + Clone,
    ) -> IbcReceiveResponse<Self::CustomMsg> {
        let handle = || -> Result<IbcReceiveResponse<Self::CustomMsg>, Self::Error> {
            let packet = Self::Packet::try_from(raw_packet.clone().into())?;

            // NOTE: The default message ack is always successful and only
            // overwritten if the submessage execution revert via the reply handler.
            // the caller MUST ENSURE that the reply is threaded through the
            // protocol.
            let execute_msg = SubMsg::reply_on_error(
                self.make_receive_phase1_execute(raw_packet)?,
                Self::RECEIVE_REPLY_ID,
            );

            Ok(IbcReceiveResponse::new()
                .set_ack(Self::ack_success().try_into()?)
                .add_event(
                    Event::new(PACKET_EVENT)
                        .add_attributes([
                            ("module", MODULE_NAME),
                            ("sender", packet.sender()),
                            ("receiver", packet.receiver()),
                            ("memo", packet.extension().clone().into().as_str()),
                            ("success", "true"),
                        ])
                        .add_attributes(packet.tokens().into_iter().map(
                            |TransferToken { denom, amount }| (format!("denom:{}", denom), amount),
                        )),
                )
                .add_submessage(execute_msg))
        };

        match handle() {
            Ok(response) => response,
            // NOTE: same branch as if the submessage fails
            Err(err) => Self::receive_error(err),
        }
    }

    fn receive_phase1_transfer(
        &mut self,
        receiver: &str,
        tokens: Vec<TransferToken>,
    ) -> Result<Vec<CosmosMsg<Self::CustomMsg>>, Self::Error>;

    fn receive_phase1(
        &mut self,
        raw_packet: impl Into<Binary>,
    ) -> Result<Response<Self::CustomMsg>, Self::Error> {
        let packet = Self::Packet::try_from(raw_packet.into())?;

        // Only the running contract is allowed to execute this message
        if self.caller() != self.self_addr() {
            return Err(ProtocolError::Unauthorized.into());
        }

        Ok(Response::new()
            .add_messages(self.receive_phase1_transfer(packet.receiver(), packet.tokens())?))
    }

    fn receive_error(error: impl Debug) -> IbcReceiveResponse<Self::CustomMsg> {
        let error = format!("{:?}", error);
        IbcReceiveResponse::new()
            .set_ack(
                Self::ack_failure(error.clone())
                    .try_into()
                    .expect("impossible"),
            )
            .add_event(Event::new(PACKET_EVENT).add_attributes([
                ("module", MODULE_NAME),
                ("success", "false"),
                ("error", &error),
            ]))
    }
}